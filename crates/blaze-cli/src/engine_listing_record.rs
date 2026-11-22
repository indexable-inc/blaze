//! A CRATE'S HEAD LISTING AS RECORDS (engine.md "An edit costs what it moved", D149 piece I2;
//! rule 51: what outlives a task is a record; rule 77: never rebuild what a record holds; rule
//! 21: its key covers every input).
//!
//! THE CAUSE IT REMOVES: a crate's listing of heads (engine_heads.rs `Listing`) was made once a
//! REQUEST (`Listings`), so every command walked the crate's items record and expansion
//! directory and read and outlined the text of every member file that holds a head
//! (`heads_of_crate`), a one-body edit included, which moves none of it.
//!
//! TWO GRAINS, both records:
//! - A FILE'S PART: the rows of one member file ([`Rows`]: its heads with their entries and
//!   cells, the heads no entry of it covers, the calls whose walk tops in it), keyed by
//!   [`part_key`]: the crate's facts, the file's [`FileFacts`] (its `Subject` row, its surface
//!   as the items record recorded it, its entries' facts) and what the crate's walk placed in
//!   it (each head's top entry, want, names and walk; each call's top entry, key and directory
//!   row). Made from the file's text alone (engine_heads.rs `file_rows`), so adding an item to
//!   one file makes that file's part and reads every other.
//! - THE CRATE'S JOIN: its parts' rows and the walk's own refusals, joined, sorted and deduped
//!   as one listing ([`join`]), keyed by [`join_key`]: the crate's facts, the digests of its
//!   items and names records, its expansion directory's rows, and every member file's
//!   [`FileFacts`]. It is read FIRST, before any walk: a one-body edit moves none of its inputs,
//!   so the listing is one read (no walk, no text, no outline).
//!
//! THE KEY: what a listing is a function of, every input named (and nothing of the machine):
//! - the tool (rule 52: a new blaze never reads an old one's rows) and this form's [`VERSION`];
//! - the crate's key as the reader's unit views it (every head name of it begins with its
//!   prefix; a block's text key is made from it, body_types/covered.rs `block_key`) and its
//!   edition (the outline parses by it);
//! - the walk's inputs: the items record (each item's kind, name, place, members, each member's
//!   outline entry and writing call, each file's recorded surface) and the names record (what
//!   `by_origin::kind_of_lib` and `item_want` read) as their bytes' digests; the expansion
//!   directory's rows (`written_path` reads each call's `call_part`: its text, entry, input
//!   start and macro name; the record key and lengths are hashed too, so a call whose
//!   expansion moved remakes rather than trusts what its directory row did not say);
//! - each file's text facts as its outline holds them ([`entries_of`]): for every entry, in
//!   order, its kind, parent, last descendant, name, disambiguator, head digest
//!   (blaze-file-items `head_of`: what `item_chain`, an unnamed entry's shown head and
//!   `covering_entry` read) and whether its body holds items, with today's surface. NO byte
//!   range is in it: a body edit that writes no item leaves it equal. A file with no outline
//!   keys by its whole text's digest, so any byte makes it again;
//! - the test plant that changes how an entry is named (engine_heads.rs `ListingsPlant`).
//!
//! THE FORM: little-endian, every count checked against the bytes left before a list is
//! reserved, every tag and text checked once at open, nothing after the end. A join is also
//! checked for its sort orders (what its readers binary-search) and for its digest, so a
//! damaged record is refused at open and made again, never read as a listing.

use core::fmt;

use blaze_drive::cells::Cell;
use blaze_drive::{Key, Kind, Name, Tool};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{WANT_BYTES, want_bytes};
use blaze_own::{AVec, Alloc};
use blaze_scope::expansion_records::{Entry, ExpansionRecordsError};

use crate::engine_tasks::Outlined;
use crate::names::expansion_texts::ExpansionTexts;

/// The form's version: in both keys' hashes and in the bytes.
pub(crate) const VERSION: u32 = 1;

/// The key domain of a crate's join.
const JOIN_KEY_DOMAIN: &str = "blaze-cli engine heads: a crate's listing record 1";
/// The key domain of a file's part.
const PART_KEY_DOMAIN: &str = "blaze-cli engine heads: a file's listing part 1";
/// The domain of a file's entries' facts ([`entries_of`]).
const ENTRIES_DOMAIN: &str = "blaze-cli engine heads: a file's entry facts 1";
/// The domain of one expansion directory row ([`call_row`]).
const CALL_ROW_DOMAIN: &str = "blaze-cli engine heads: an expansion directory row 1";
/// The domain of a crate's expansion directory ([`directory_digest`]).
const DIRECTORY_DOMAIN: &str = "blaze-cli engine heads: an expansion directory 1";
/// The domain of a record's section bytes ([`bytes_digest`]).
const BYTES_DOMAIN: &str = "blaze-cli engine heads: a listing input's bytes 1";
/// The domain of a listing's digest ([`rows_digest`]): what two makes are compared by and what
/// a join's bytes are checked against. 2: a head's macro (`by`) and the absence of a member
/// name are in it.
const LISTING_DOMAIN: &str = "blaze-cli engine heads: a crate's listing 2";

/// The bytes a join begins with.
const JOIN_MAGIC: [u8; 8] = *b"BLZLSTJ\x01";
/// The bytes a part begins with.
const PART_MAGIC: [u8; 8] = *b"BLZLSTP\x01";

/// A name's bytes into a digest (its kind's wire byte, its subject): the one form both keys
/// and every listing digest take (engine_heads.rs `subjects_digest` too).
pub(crate) fn name_part(h: &mut Hasher, name: Name) {
    h.part(&[name.kind as u8]).part(&name.subject.0);
}

/// One head of a listing: the entry whose head task freezes it, its cell there, its head
/// name, the file it is placed in, its want and item, whether its entry's body holds items,
/// and the words a refusal names it by (engine_heads.rs `HeadLabel`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head<'r> {
    pub(crate) entry: Name,
    pub(crate) cell: Cell,
    pub(crate) sig: Name,
    pub(crate) file: Name,
    pub(crate) want: Want,
    pub(crate) item: Cell,
    pub(crate) body_holds_items: bool,
    pub(crate) name: &'r str,
    pub(crate) member: Option<&'r str>,
    pub(crate) entry_name: &'r str,
    /// `None`: written in its file; `Some(None)`: in a body's block; `Some(Some(m))`: by `m!`.
    pub(crate) by: Option<Option<&'r str>>,
}

/// A listing's rows, a part's or a join's: heads, heads no task holds (and why), and the calls
/// whose walk tops under an entry.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Rows<'r, 'a> {
    pub(crate) heads: AVec<'a, Head<'r>>,
    pub(crate) unlisted: AVec<'a, (Name, &'r str)>,
    pub(crate) calls: AVec<'a, (Name, blaze_id::ItemKey)>,
}

impl<'r, 'a> Rows<'r, 'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> Rows<'r, 'a> {
        Rows { heads: AVec::new_in(alloc), unlisted: AVec::new_in(alloc), calls: AVec::new_in(alloc) }
    }

    /// Every row of `other` after these.
    pub(crate) fn append(&mut self, other: &Rows<'r, '_>) {
        self.heads.extend_from_slice(&other.heads);
        self.unlisted.extend_from_slice(&other.unlisted);
        self.calls.extend_from_slice(&other.calls);
    }
}

/// A crate's listing, joined: its rows sorted (heads by (entry, cell, head name), one each; the
/// rest by head name and by (entry, call)), `by_sig` the least head of each head name, and the
/// digest of every row.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Joined<'r, 'a> {
    pub(crate) rows: Rows<'r, 'a>,
    pub(crate) by_sig: AVec<'a, u32>,
    pub(crate) digest: ContentId,
}

/// The order heads are sorted in before their dedup: every field, so equal keys keep the least
/// (rule 25: no order of the walk or of the parts shows).
#[expect(clippy::type_complexity, reason = "every field of a head, in its sort order")]
fn head_order<'h>(one: &Head<'h>) -> (Name, Cell, Name, [u8; WANT_BYTES], Cell, Name, bool, &'h str, Option<&'h str>, &'h str, Option<Option<&'h str>>) {
    (one.entry, one.cell, one.sig, want_bytes(one.want), one.item, one.file, one.body_holds_items, one.name, one.member, one.entry_name, one.by)
}

/// `rows` joined: sorted, deduped, indexed by head name and digested (the one join, of a
/// crate made now and checked again at a join's open).
pub(crate) fn join<'r, 'a>(mut rows: Rows<'r, 'a>, alloc: Alloc<'a>) -> Joined<'r, 'a> {
    rows.heads.sort_unstable_by(|a, b| head_order(a).cmp(&head_order(b)));
    rows.heads.dedup_by_key(|one| (one.entry, one.cell, one.sig));
    let mut by_sig: AVec<'a, u32> = AVec::with_capacity_in(rows.heads.len(), alloc);
    by_sig.extend((0..rows.heads.len()).map(|at| u32::try_from(at).unwrap_or(u32::MAX)));
    let heads = &rows.heads;
    by_sig.sort_unstable_by_key(|at| {
        let one = &heads[*at as usize];
        (one.sig, one.entry, one.file, one.cell)
    });
    by_sig.dedup_by_key(|at| heads[*at as usize].sig);
    rows.unlisted.sort_unstable();
    rows.unlisted.dedup_by_key(|(sig, _)| *sig);
    rows.calls.sort_unstable();
    rows.calls.dedup();
    let digest = rows_digest(&rows);
    Joined { rows, by_sig, digest }
}

/// The digest of every row, in order.
pub(crate) fn rows_digest(rows: &Rows<'_, '_>) -> ContentId {
    let mut h = Hasher::new(LISTING_DOMAIN);
    h.part(&u64::try_from(rows.heads.len()).unwrap_or(u64::MAX).to_le_bytes());
    for one in rows.heads.iter() {
        name_part(&mut h, one.entry);
        h.part(&one.cell.0);
        name_part(&mut h, one.sig);
        name_part(&mut h, one.file);
        h.part(&want_bytes(one.want)).part(&one.item.0);
        h.update(&[u8::from(one.body_holds_items)]);
        h.part(one.name.as_bytes()).part(one.entry_name.as_bytes());
        match one.member {
            None => h.update(&[0]),
            Some(member) => h.update(&[1]).part(member.as_bytes()),
        };
        match one.by {
            None => h.update(&[0]),
            Some(None) => h.update(&[1]),
            Some(Some(by)) => h.update(&[2]).part(by.as_bytes()),
        };
    }
    h.part(&u64::try_from(rows.unlisted.len()).unwrap_or(u64::MAX).to_le_bytes());
    for (sig, why) in rows.unlisted.iter() {
        name_part(&mut h, *sig);
        h.part(why.as_bytes());
    }
    h.part(&u64::try_from(rows.calls.len()).unwrap_or(u64::MAX).to_le_bytes());
    for (entry, call) in rows.calls.iter() {
        name_part(&mut h, *entry);
        h.part(&call.0.0);
    }
    h.finish()
}

// THE KEYS.

/// What a member file contributes to its crate's join and its own part (module doc, THE KEY).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileFacts {
    /// Its index among the items record's files.
    pub(crate) file: u32,
    /// Its logical module slot (the outline's task name) and its physical signature file.
    pub(crate) slot: Name,
    pub(crate) signature_file: ContentId,
    /// Its surface as the items record's file row holds it; `None`: no such row.
    pub(crate) recorded: Option<ContentId>,
    /// Its entries' facts and today's surface ([`entries_of`]).
    pub(crate) entries: ContentId,
}

/// What every key of one crate holds (module doc, THE KEY).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CrateFacts {
    pub(crate) tool: Tool,
    /// The crate's key as the reader's unit views it.
    pub(crate) krate: ContentId,
    /// Its edition's byte; `None`: its records give none (no file is outlined then).
    pub(crate) edition: Option<u8>,
    /// The test plant that changes an entry's name (engine_heads.rs `ListingsPlant::key_tag`).
    pub(crate) plant: u8,
}

/// The inputs of a crate's join.
#[derive(Clone, Copy)]
pub(crate) struct JoinKey<'p> {
    pub(crate) krate: CrateFacts,
    /// The digests of its items and names records' bytes ([`bytes_digest`]).
    pub(crate) items: ContentId,
    pub(crate) names: ContentId,
    /// Its expansion directory ([`directory_digest`]).
    pub(crate) directory: ContentId,
    /// Each member file of the crate, in its `Subjects` order (by file index).
    pub(crate) files: &'p [FileFacts],
}

/// A head the crate's walk placed in a file: its top entry there, its want, its item's and an
/// impl fn's names, and for a written head its walk's steps and innermost macro.
#[derive(Clone, Copy)]
pub(crate) struct PlacedHead<'n> {
    pub(crate) top: u32,
    pub(crate) want: Want,
    pub(crate) name: &'n str,
    pub(crate) member: Option<&'n str>,
    pub(crate) walk: Option<(ContentId, Option<&'n str>)>,
}

/// A call whose walk tops at entry `top` of a file, with its directory row's digest.
#[derive(Clone, Copy)]
pub(crate) struct PlacedCall {
    pub(crate) top: u32,
    pub(crate) call: blaze_id::ItemKey,
    pub(crate) row: ContentId,
}

/// The inputs of one file's part.
#[derive(Clone, Copy)]
pub(crate) struct PartKey<'p, 'n> {
    pub(crate) krate: CrateFacts,
    pub(crate) file: FileFacts,
    pub(crate) heads: &'p [PlacedHead<'n>],
    pub(crate) calls: &'p [PlacedCall],
}

fn crate_part(h: &mut Hasher, krate: &CrateFacts) {
    h.update(&VERSION.to_le_bytes());
    h.id(&krate.tool.0).id(&krate.krate);
    match krate.edition {
        None => h.update(&[0]),
        Some(edition) => h.update(&[1, edition]),
    };
    h.update(&[krate.plant]);
}

fn file_part(h: &mut Hasher, file: &FileFacts) {
    h.update(&file.file.to_le_bytes());
    name_part(h, file.slot);
    h.id(&file.signature_file);
    match file.recorded {
        None => h.update(&[0]),
        Some(surface) => h.update(&[1]).update(&surface.0),
    };
    h.id(&file.entries);
}

/// The store key of a crate's join.
pub(crate) fn join_key(parts: &JoinKey<'_>) -> Key {
    let mut h = Hasher::new(JOIN_KEY_DOMAIN);
    crate_part(&mut h, &parts.krate);
    h.id(&parts.items).id(&parts.names).id(&parts.directory);
    h.update(&u64::try_from(parts.files.len()).unwrap_or(u64::MAX).to_le_bytes());
    for file in parts.files {
        file_part(&mut h, file);
    }
    Key(h.finish())
}

/// The store key of one file's part.
pub(crate) fn part_key(parts: &PartKey<'_, '_>) -> Key {
    let mut h = Hasher::new(PART_KEY_DOMAIN);
    crate_part(&mut h, &parts.krate);
    file_part(&mut h, &parts.file);
    h.update(&u64::try_from(parts.heads.len()).unwrap_or(u64::MAX).to_le_bytes());
    for one in parts.heads {
        h.update(&one.top.to_le_bytes()).update(&want_bytes(one.want)).part(one.name.as_bytes());
        match one.member {
            None => h.update(&[0]),
            Some(member) => h.update(&[1]).part(member.as_bytes()),
        };
        match one.walk {
            None => h.update(&[0]),
            Some((steps, None)) => h.update(&[1]).update(&steps.0),
            Some((steps, Some(by))) => h.update(&[2]).update(&steps.0).part(by.as_bytes()),
        };
    }
    h.update(&u64::try_from(parts.calls.len()).unwrap_or(u64::MAX).to_le_bytes());
    for one in parts.calls {
        h.update(&one.top.to_le_bytes()).update(&one.call.0.0).update(&one.row.0);
    }
    Key(h.finish())
}

/// The digest of a record section's bytes (the items or names record a walk reads).
pub(crate) fn bytes_digest(bytes: &[u8]) -> ContentId {
    let mut h = Hasher::new(BYTES_DOMAIN);
    h.part(bytes);
    h.finish()
}

/// One directory row's digest: every column of it, the call part the walk reads among them.
pub(crate) fn call_row(entry: &Entry<'_>) -> ContentId {
    let mut h = Hasher::new(CALL_ROW_DOMAIN);
    h.update(&entry.call.0.0).update(&entry.record.0).update(&entry.text_len.to_le_bytes()).update(&entry.column_len.to_le_bytes()).part(entry.call_part);
    h.finish()
}

/// The digest of every row of a crate's expansion directory, in its order. `Err`: a row that
/// does not read, so no key can be made (the listing is made, and not stored).
pub(crate) fn directory_digest(texts: &ExpansionTexts<'_>) -> Result<ContentId, ExpansionRecordsError> {
    let mut h = Hasher::new(DIRECTORY_DOMAIN);
    h.update(&u64::try_from(texts.len()).unwrap_or(u64::MAX).to_le_bytes());
    for at in 0..texts.len() {
        let call = texts.call_at(at)?;
        match texts.entry(call)? {
            Some(entry) => h.update(&[1]).update(&call_row(&entry).0),
            None => h.update(&[0]).update(&call.0.0),
        };
    }
    Ok(h.finish())
}

/// The facts of outlined file `one` a listing reads of its text (module doc, THE KEY): for
/// every entry, in order, its kind, parent, last descendant, name, disambiguator, head digest
/// and whether its body holds items; and today's surface. No byte range. A file whose outline
/// was not made (or does not open) gives its whole text's digest instead.
pub(crate) fn entries_of(one: &Outlined<'_>) -> ContentId {
    let mut h = Hasher::new(ENTRIES_DOMAIN);
    h.update(&[one.edition]);
    let view = match &one.made {
        Ok(record) => record.payload_bytes().ok().and_then(|payload| blaze_surface::FileItemsView::read(payload).ok()).map(|view| (view, crate::engine_tasks::surface_of(record))),
        Err(_) => None,
    };
    let Some((view, surface)) = view else {
        h.update(&[0]).id(&one.outline_source);
        return h.finish();
    };
    h.update(&[1]);
    match surface {
        None => h.update(&[0]),
        Some(surface) => h.update(&[1]).update(&surface.0),
    };
    h.update(&u64::try_from(view.len()).unwrap_or(u64::MAX).to_le_bytes());
    for entry in view.items() {
        let parent = entry.parent.map_or(u32::MAX, |up| up.0);
        h.update(&[entry.kind().byte()]).update(&parent.to_le_bytes()).update(&entry.end.0.to_le_bytes());
        h.part(entry.name.as_bytes()).update(&entry.disambiguator.to_le_bytes());
        h.id(&blaze_file_items::head_of(&entry.item));
        let holds = matches!(
            entry.item.detail,
            blaze_surface::ItemDetail::Fn { body_holds_items: true } | blaze_surface::ItemDetail::Const { body_holds_items: true } | blaze_surface::ItemDetail::Static { body_holds_items: true }
        );
        h.update(&[u8::from(holds)]);
    }
    h.finish()
}

// THE FORM.

/// A want's bytes back ([`want_bytes`]' layout: its kind, its item, an impl fn's key).
fn want_of(b: &[u8], at: usize) -> Result<Want, ListingRefused> {
    let mut lib = [0u8; 32];
    lib.copy_from_slice(&b[1..33]);
    let lib = blaze_id::ItemKey(ContentId(lib));
    let mut member = [0u8; 16];
    member.copy_from_slice(&b[33..WANT_BYTES]);
    Ok(match b[0] {
        1 => Want::Head(lib),
        2 => Want::Adt(lib),
        3 => Want::Impl(lib),
        4 => Want::Fn(lib),
        5 => Want::Members(lib),
        6 => Want::MemberFn(lib, blaze_typeck::ItemKey(u128::from_le_bytes(member))),
        found => return Err(ListingRefused::Tag { at, what: "a want's kind", found }),
    })
}

fn put_name(into: &mut AVec<'_, u8>, name: Name) {
    into.push(name.kind as u8);
    into.extend_from_slice(&name.subject.0);
}

fn put_text(into: &mut AVec<'_, u8>, text: &str) {
    into.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_le_bytes());
    into.extend_from_slice(text.as_bytes());
}

fn put_count(into: &mut AVec<'_, u8>, n: usize) {
    into.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
}

fn put_rows(rows: &Rows<'_, '_>, into: &mut AVec<'_, u8>) {
    put_count(into, rows.heads.len());
    for one in rows.heads.iter() {
        put_name(into, one.entry);
        into.extend_from_slice(&one.cell.0);
        put_name(into, one.sig);
        put_name(into, one.file);
        into.extend_from_slice(&want_bytes(one.want));
        into.extend_from_slice(&one.item.0);
        into.push(u8::from(one.body_holds_items));
        put_text(into, one.name);
        put_text(into, one.entry_name);
        match one.member {
            None => into.push(0),
            Some(member) => {
                into.push(1);
                put_text(into, member);
            }
        }
        match one.by {
            None => into.push(0),
            Some(None) => into.push(1),
            Some(Some(by)) => {
                into.push(2);
                put_text(into, by);
            }
        }
    }
    put_count(into, rows.unlisted.len());
    for (sig, why) in rows.unlisted.iter() {
        put_name(into, *sig);
        put_text(into, why);
    }
    put_count(into, rows.calls.len());
    for (entry, call) in rows.calls.iter() {
        put_name(into, *entry);
        into.extend_from_slice(&call.0.0);
    }
}

/// A file's part as bytes, after what `into` holds.
pub(crate) fn encode_part(rows: &Rows<'_, '_>, into: &mut AVec<'_, u8>) {
    into.extend_from_slice(&PART_MAGIC);
    into.extend_from_slice(&VERSION.to_le_bytes());
    put_rows(rows, into);
}

/// A crate's join as bytes, after what `into` holds.
pub(crate) fn encode_join(joined: &Joined<'_, '_>, into: &mut AVec<'_, u8>) {
    into.extend_from_slice(&JOIN_MAGIC);
    into.extend_from_slice(&VERSION.to_le_bytes());
    put_rows(&joined.rows, into);
    put_count(into, joined.by_sig.len());
    for at in joined.by_sig.iter() {
        into.extend_from_slice(&at.to_le_bytes());
    }
    into.extend_from_slice(&joined.digest.0);
}

/// Why a listing record's bytes do not open: each names the byte offset it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListingRefused {
    /// The bytes do not begin with the form's magic (`what`: a part's or a join's).
    Magic { what: &'static str },
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
    /// A join's rows out of the order its readers search them in (`what`), at row `row`.
    Order { what: &'static str, row: usize },
    /// A join's head-name index names head `index` of `heads`.
    Index { index: u32, heads: usize },
    /// A join whose rows do not hash to the digest it holds.
    Digest,
    /// Bytes left after the record's end.
    Trailing { at: usize, left: usize },
}

impl fmt::Display for ListingRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListingRefused::Magic { what } => write!(f, "{what} of a crate's head listing does not begin with its form's magic (engine_listing_record.rs)"),
            ListingRefused::Version { found } => write!(f, "a head listing record is of form {found}, not {VERSION} (engine_listing_record.rs)"),
            ListingRefused::Short { at, what, need } => write!(f, "a head listing record ends at byte {at}, where its {what} needs {need} more (engine_listing_record.rs)"),
            ListingRefused::Count { at, what, count } => write!(f, "a head listing record counts {count} {what} at byte {at}, more than its bytes hold (engine_listing_record.rs)"),
            ListingRefused::Utf8 { at } => write!(f, "a head listing record's text at byte {at} is not UTF-8 (engine_listing_record.rs)"),
            ListingRefused::Tag { at, what, found } => write!(f, "a head listing record's {what} tag at byte {at} is {found}, which no form has (engine_listing_record.rs)"),
            ListingRefused::Order { what, row } => write!(f, "a crate's head listing holds its {what} out of order at row {row} (engine_listing_record.rs `join`)"),
            ListingRefused::Index { index, heads } => write!(f, "a crate's head listing indexes head {index} of {heads} (engine_listing_record.rs `join`)"),
            ListingRefused::Digest => write!(f, "a crate's head listing's rows do not hash to the digest it holds (engine_listing_record.rs `rows_digest`)"),
            ListingRefused::Trailing { at, left } => write!(f, "a head listing record has {left} bytes after its end at byte {at} (engine_listing_record.rs)"),
        }
    }
}

/// The least bytes of one head, unlisted head and call row (each text its length only).
const HEAD_LEAST: usize = 33 + 16 + 33 + 33 + WANT_BYTES + 16 + 1 + 4 + 4 + 1 + 1;
const UNLISTED_LEAST: usize = 33 + 4;
const CALL_LEAST: usize = 33 + 32;

/// A cursor over a record's bytes: every read checks its length once.
struct Cursor<'r> {
    bytes: &'r [u8],
    at: usize,
}

impl<'r> Cursor<'r> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'r [u8], ListingRefused> {
        let left = self.bytes.len() - self.at;
        if left < n {
            return Err(ListingRefused::Short { at: self.at, what, need: n - left });
        }
        let out = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, ListingRefused> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, ListingRefused> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn id(&mut self, what: &'static str) -> Result<ContentId, ListingRefused> {
        let mut words = [0u8; 32];
        words.copy_from_slice(self.take(32, what)?);
        Ok(ContentId(words))
    }

    fn cell(&mut self, what: &'static str) -> Result<Cell, ListingRefused> {
        let mut words = [0u8; 16];
        words.copy_from_slice(self.take(16, what)?);
        Ok(Cell(words))
    }

    fn name(&mut self, what: &'static str) -> Result<Name, ListingRefused> {
        let at = self.at;
        let byte = self.u8(what)?;
        let kind = Kind::from_u8(byte).ok_or(ListingRefused::Tag { at, what, found: byte })?;
        Ok(Name::new(kind, self.id(what)?))
    }

    fn text(&mut self, what: &'static str) -> Result<&'r str, ListingRefused> {
        let len = self.u32(what)?;
        let at = self.at;
        let bytes = self.take(usize::try_from(len).unwrap_or(usize::MAX), what)?;
        core::str::from_utf8(bytes).map_err(|_| ListingRefused::Utf8 { at })
    }

    /// A count of entries each at least `least` bytes long, checked against what is left.
    fn count(&mut self, least: usize, what: &'static str) -> Result<usize, ListingRefused> {
        let at = self.at;
        let count = self.u32(what)?;
        let n = usize::try_from(count).unwrap_or(usize::MAX);
        if n.saturating_mul(least) > self.bytes.len() - self.at {
            return Err(ListingRefused::Count { at, what, count });
        }
        Ok(n)
    }

    fn head(&mut self) -> Result<Head<'r>, ListingRefused> {
        let entry = self.name("head's entry")?;
        let cell = self.cell("head's cell")?;
        let sig = self.name("head's name")?;
        let file = self.name("head's file")?;
        let at = self.at;
        let want = want_of(self.take(WANT_BYTES, "head's want")?, at)?;
        let item = self.cell("head's item")?;
        let at = self.at;
        let body_holds_items = match self.u8("head's body flag")? {
            0 => false,
            1 => true,
            found => return Err(ListingRefused::Tag { at, what: "a head's body flag", found }),
        };
        let name = self.text("head's item name")?;
        let entry_name = self.text("head's entry name")?;
        let at = self.at;
        let member = match self.u8("head's member tag")? {
            0 => None,
            1 => Some(self.text("head's member name")?),
            found => return Err(ListingRefused::Tag { at, what: "a head's member", found }),
        };
        let at = self.at;
        let by = match self.u8("head's macro tag")? {
            0 => None,
            1 => Some(None),
            2 => Some(Some(self.text("head's macro")?)),
            found => return Err(ListingRefused::Tag { at, what: "a head's macro", found }),
        };
        Ok(Head { entry, cell, sig, file, want, item, body_holds_items, name, member, entry_name, by })
    }

    fn rows<'a>(&mut self, alloc: Alloc<'a>) -> Result<Rows<'r, 'a>, ListingRefused> {
        let n = self.count(HEAD_LEAST, "heads")?;
        let mut heads = AVec::with_capacity_in(n, alloc);
        for _ in 0..n {
            heads.push(self.head()?);
        }
        let n = self.count(UNLISTED_LEAST, "unlisted heads")?;
        let mut unlisted = AVec::with_capacity_in(n, alloc);
        for _ in 0..n {
            let sig = self.name("unlisted head's name")?;
            unlisted.push((sig, self.text("unlisted head's reason")?));
        }
        let n = self.count(CALL_LEAST, "calls")?;
        let mut calls = AVec::with_capacity_in(n, alloc);
        for _ in 0..n {
            let entry = self.name("call's entry")?;
            calls.push((entry, blaze_id::ItemKey(self.id("call's key")?)));
        }
        Ok(Rows { heads, unlisted, calls })
    }

    fn header(&mut self, magic: &[u8; 8], what: &'static str) -> Result<(), ListingRefused> {
        if self.take(magic.len(), "magic").map_err(|_| ListingRefused::Magic { what })? != &magic[..] {
            return Err(ListingRefused::Magic { what });
        }
        let found = self.u32("version")?;
        if found != VERSION {
            return Err(ListingRefused::Version { found });
        }
        Ok(())
    }

    fn end(&self) -> Result<(), ListingRefused> {
        match self.bytes.len() - self.at {
            0 => Ok(()),
            left => Err(ListingRefused::Trailing { at: self.at, left }),
        }
    }
}

/// A file's part in `bytes`, its texts read in place and its lists in `alloc`.
pub(crate) fn open_part<'r, 'a>(bytes: &'r [u8], alloc: Alloc<'a>) -> Result<Rows<'r, 'a>, ListingRefused> {
    let mut c = Cursor { bytes, at: 0 };
    c.header(&PART_MAGIC, "a file's part")?;
    let rows = c.rows(alloc)?;
    c.end()?;
    Ok(rows)
}

/// A crate's join in `bytes`, its texts read in place and its lists in `alloc`: its sort
/// orders, its index and its digest checked here, once.
pub(crate) fn open_join<'r, 'a>(bytes: &'r [u8], alloc: Alloc<'a>) -> Result<Joined<'r, 'a>, ListingRefused> {
    let mut c = Cursor { bytes, at: 0 };
    c.header(&JOIN_MAGIC, "a crate's join")?;
    let rows = c.rows(alloc)?;
    let n = c.count(4, "head-name index rows")?;
    let mut by_sig = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        by_sig.push(c.u32("head-name index row")?);
    }
    let digest = c.id("digest")?;
    c.end()?;
    let heads = &rows.heads;
    if let Some(row) = (1..heads.len()).find(|at| (heads[at - 1].entry, heads[at - 1].cell, heads[at - 1].sig) >= (heads[*at].entry, heads[*at].cell, heads[*at].sig)) {
        return Err(ListingRefused::Order { what: "heads", row });
    }
    if let Some(index) = by_sig.iter().copied().find(|at| *at as usize >= heads.len()) {
        return Err(ListingRefused::Index { index, heads: heads.len() });
    }
    if let Some(row) = (1..by_sig.len()).find(|at| heads[by_sig[at - 1] as usize].sig >= heads[by_sig[*at] as usize].sig) {
        return Err(ListingRefused::Order { what: "head-name index", row });
    }
    if let Some(row) = (1..rows.unlisted.len()).find(|at| rows.unlisted[at - 1].0 >= rows.unlisted[*at].0) {
        return Err(ListingRefused::Order { what: "unlisted heads", row });
    }
    if let Some(row) = (1..rows.calls.len()).find(|at| rows.calls[at - 1] >= rows.calls[*at]) {
        return Err(ListingRefused::Order { what: "calls", row });
    }
    if rows_digest(&rows) != digest {
        return Err(ListingRefused::Digest);
    }
    Ok(Joined { rows, by_sig, digest })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(kind: Kind, v: u8) -> Name {
        Name::new(kind, ContentId([v; 32]))
    }

    fn lib(v: u8) -> blaze_id::ItemKey {
        blaze_id::ItemKey(ContentId([v; 32]))
    }

    /// Every want's form, so each tag round-trips.
    fn wants() -> [Want; 6] {
        [Want::Head(lib(1)), Want::Adt(lib(2)), Want::Impl(lib(3)), Want::Fn(lib(4)), Want::Members(lib(5)), Want::MemberFn(lib(6), blaze_typeck::ItemKey(u128::MAX - 7))]
    }

    /// Rows with every part: a head of each want, with and without a member and each kind of
    /// macro tag; heads under one (entry, cell, name) twice (the dedup keeps the least); two
    /// unlisted heads under one name; a call twice.
    fn rows<'a>(alloc: Alloc<'a>) -> Rows<'static, 'a> {
        let mut out = Rows::new(alloc);
        for (at, want) in wants().into_iter().enumerate() {
            let v = u8::try_from(at).unwrap_or(0);
            let by = match at % 3 {
                0 => None,
                1 => Some(None),
                _ => Some(Some("serde::Serialize")),
            };
            out.heads.push(Head {
                entry: name(Kind::Signature, 10 + v),
                cell: Cell([v; 16]),
                sig: name(Kind::Signature, 30 - v),
                file: name(Kind::FileItems, 1),
                want,
                item: Cell([40 + v; 16]),
                body_holds_items: at % 2 == 0,
                name: "Point",
                member: (at == 5).then_some("measure"),
                entry_name: "impl Measure for Point",
                by,
            });
        }
        let twice = Head { name: "Other", item: Cell([99; 16]), ..out.heads[2] };
        out.heads.push(twice);
        out.unlisted.push((name(Kind::Signature, 50), "no entry covers it"));
        out.unlisted.push((name(Kind::Signature, 50), "a later reason"));
        out.unlisted.push((name(Kind::Signature, 49), "uncovered"));
        out.calls.push((name(Kind::Signature, 11), lib(9)));
        out.calls.push((name(Kind::Signature, 11), lib(9)));
        out.calls.push((name(Kind::Signature, 10), lib(8)));
        out
    }

    fn part_bytes<'a>(rows: &Rows<'_, '_>, alloc: Alloc<'a>) -> AVec<'a, u8> {
        let mut out = AVec::new_in(alloc);
        encode_part(rows, &mut out);
        out
    }

    fn join_bytes<'a>(joined: &Joined<'_, '_>, alloc: Alloc<'a>) -> AVec<'a, u8> {
        let mut out = AVec::new_in(alloc);
        encode_join(joined, &mut out);
        out
    }

    /// Rule 26, restore equals fresh: a part and a join with every kind of row open to what was
    /// encoded and encode again to the same bytes; the join's digest is its rows' again. The
    /// join is sorted and deduped: one head per (entry, cell, name), the least kept; one
    /// unlisted row per name; each call once. CONTROL: a join one head's macro apart has
    /// another digest and other bytes, so the comparison sees the part most easily lost.
    #[test]
    fn a_part_and_a_join_round_trip_in_bytes_and_digest() {
        let heap = blaze_own::testing::heap();
        let g = heap.alloc();
        let made = rows(g);
        let bytes = part_bytes(&made, g);
        let back = open_part(&bytes, g).expect("the part opens");
        assert_eq!(back, made, "the opened part is the part encoded");
        assert_eq!(&part_bytes(&back, g)[..], &bytes[..], "a part: restore equals fresh in bytes");

        let joined = join(rows(g), g);
        assert_eq!(joined.rows.heads.len(), 6, "the head twice under one key is kept once: {:?}", joined.rows.heads);
        assert!(joined.rows.heads.iter().all(|one| one.name == "Point"), "the least of two heads under one key is kept");
        assert_eq!(joined.rows.unlisted.len(), 2, "one unlisted row a head name");
        assert_eq!(joined.rows.calls.len(), 2, "each call once");
        assert_eq!(joined.by_sig.len(), 6, "one index row a head name");
        let bytes = join_bytes(&joined, g);
        let back = open_join(&bytes, g).expect("the join opens");
        assert_eq!(back, joined, "the opened join is the join encoded");
        assert_eq!(back.digest, rows_digest(&back.rows), "restore equals fresh in digest");
        assert_eq!(&join_bytes(&back, g)[..], &bytes[..], "a join: restore equals fresh in bytes");
        // A join of the parts in another order is the same bytes (rule 25).
        let mut reversed = rows(g);
        reversed.heads.reverse();
        reversed.unlisted.reverse();
        reversed.calls.reverse();
        assert_eq!(&join_bytes(&join(reversed, g), g)[..], &bytes[..], "no order of the parts shows in the join");

        // CONTROL.
        let mut other = rows(g);
        other.heads[0].by = Some(Some("m"));
        let other = join(other, g);
        assert_ne!(other.digest, joined.digest, "a moved macro is another digest");
        assert_ne!(&join_bytes(&other, g)[..], &bytes[..], "and other bytes");
    }

    /// Every length is checked once at open: a record cut at each byte, a count past its bytes,
    /// a trailing byte, another version, a stray kind byte, rows out of order and a digest
    /// that is not the rows' are each refused by name. CONTROL: the whole records open, so each
    /// refusal is the damage's.
    #[test]
    fn a_damaged_record_is_refused_at_open() {
        let heap = blaze_own::testing::heap();
        let g = heap.alloc();
        let part = part_bytes(&rows(g), g);
        let joined = join_bytes(&join(rows(g), g), g);
        assert!(open_part(&part, g).is_ok() && open_join(&joined, g).is_ok(), "CONTROL: the whole records open");
        for cut in 0..part.len() {
            assert!(open_part(&part[..cut], g).is_err(), "a part cut at byte {cut} of {} is refused", part.len());
        }
        for cut in 0..joined.len() {
            assert!(open_join(&joined[..cut], g).is_err(), "a join cut at byte {cut} of {} is refused", joined.len());
        }
        assert!(matches!(open_part(&joined, g), Err(ListingRefused::Magic { .. })), "a join is no part");
        assert!(matches!(open_join(&part, g), Err(ListingRefused::Magic { .. })), "a part is no join");
        let mut long = AVec::new_in(g);
        long.extend_from_slice(&joined);
        long.push(0);
        assert!(matches!(open_join(&long, g), Err(ListingRefused::Trailing { left: 1, .. })), "a trailing byte is refused");
        let mut version = AVec::new_in(g);
        version.extend_from_slice(&joined);
        version[8] = 2;
        assert_eq!(open_join(&version, g), Err(ListingRefused::Version { found: 2 }), "another form is refused, never read");
        let mut counted = AVec::new_in(g);
        counted.extend_from_slice(&part);
        counted[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(open_part(&counted, g), Err(ListingRefused::Count { what: "heads", .. })), "a count past the bytes is refused before any list is reserved");
        let mut kind = AVec::new_in(g);
        kind.extend_from_slice(&part);
        kind[16] = 0;
        assert!(matches!(open_part(&kind, g), Err(ListingRefused::Tag { at: 16, .. })), "a name's kind byte no kind has is refused");
        // A flipped byte of the first head's item name ("Point"): its text still reads, so the
        // digest is what refuses it.
        let name_at = 16 + 33 + 16 + 33 + 33 + WANT_BYTES + 16 + 1 + 4;
        let mut flipped = AVec::new_in(g);
        flipped.extend_from_slice(&joined);
        flipped[name_at] = b'Q';
        assert_eq!(open_join(&flipped, g), Err(ListingRefused::Digest), "rows that are not the digest's are refused");
        // Two heads swapped, the digest made again over the swapped rows: the order refuses it.
        let mut swapped = join(rows(g), g);
        swapped.rows.heads.swap(0, 1);
        swapped.digest = rows_digest(&swapped.rows);
        assert!(matches!(open_join(&join_bytes(&swapped, g), g), Err(ListingRefused::Order { what: "heads", row: 1 })), "heads out of order are refused");
    }

    fn facts(file: u32, entries: u8) -> FileFacts {
        FileFacts { file, slot: name(Kind::FileItems, 2), signature_file: ContentId([3; 32]), recorded: Some(ContentId([4; 32])), entries: ContentId([entries; 32]) }
    }

    fn krate() -> CrateFacts {
        CrateFacts { tool: Tool(ContentId([5; 32])), krate: ContentId([6; 32]), edition: Some(3), plant: 0 }
    }

    /// The input of `key_of` over a join's parts that does not move it, or `None`.
    fn join_unmoved(key_of: &dyn Fn(&JoinKey<'_>) -> Key) -> Option<&'static str> {
        let files = [facts(0, 7), facts(1, 8)];
        let base = JoinKey { krate: krate(), items: ContentId([9; 32]), names: ContentId([10; 32]), directory: ContentId([11; 32]), files: &files };
        let at = key_of(&base);
        let body_holds = [facts(0, 7), facts(1, 12)];
        let mut recorded = [facts(0, 7), facts(1, 8)];
        recorded[1].recorded = None;
        let mut slot = [facts(0, 7), facts(1, 8)];
        slot[0].slot = name(Kind::FileItems, 13);
        let mut signature_file = [facts(0, 7), facts(1, 8)];
        signature_file[0].signature_file = ContentId([14; 32]);
        let fewer = [facts(0, 7)];
        let moved: [(&'static str, JoinKey<'_>); 12] = [
            ("the tool", JoinKey { krate: CrateFacts { tool: Tool(ContentId([15; 32])), ..krate() }, ..base }),
            ("the crate", JoinKey { krate: CrateFacts { krate: ContentId([16; 32]), ..krate() }, ..base }),
            ("the edition", JoinKey { krate: CrateFacts { edition: Some(2), ..krate() }, ..base }),
            ("the plant", JoinKey { krate: CrateFacts { plant: 1, ..krate() }, ..base }),
            ("the items record", JoinKey { items: ContentId([17; 32]), ..base }),
            ("the names record", JoinKey { names: ContentId([18; 32]), ..base }),
            ("the expansion directory", JoinKey { directory: ContentId([19; 32]), ..base }),
            ("a file's entry facts", JoinKey { files: &body_holds, ..base }),
            ("a file's recorded surface", JoinKey { files: &recorded, ..base }),
            ("a file's slot", JoinKey { files: &slot, ..base }),
            ("a file's signature file", JoinKey { files: &signature_file, ..base }),
            ("the files", JoinKey { files: &fewer, ..base }),
        ];
        moved.iter().find(|(_, parts)| key_of(parts) == at).map(|(what, _)| *what)
    }

    /// The input of `key_of` over a part's parts that does not move it, or `None`.
    fn part_unmoved(key_of: &dyn Fn(&PartKey<'_, '_>) -> Key) -> Option<&'static str> {
        let head = PlacedHead { top: 3, want: Want::Fn(lib(1)), name: "f", member: None, walk: None };
        let heads = [head];
        let calls = [PlacedCall { top: 2, call: lib(2), row: ContentId([20; 32]) }];
        let base = PartKey { krate: krate(), file: facts(0, 7), heads: &heads, calls: &calls };
        let at = key_of(&base);
        let top = [PlacedHead { top: 4, ..head }];
        let want = [PlacedHead { want: Want::Adt(lib(1)), ..head }];
        let named = [PlacedHead { name: "g", ..head }];
        let member = [PlacedHead { member: Some("f"), ..head }];
        let walked = [PlacedHead { walk: Some((ContentId([21; 32]), None)), ..head }];
        let by = [PlacedHead { walk: Some((ContentId([21; 32]), Some("m"))), ..head }];
        let row = [PlacedCall { row: ContentId([22; 32]), ..calls[0] }];
        let call = [PlacedCall { call: lib(3), ..calls[0] }];
        let moved: [(&'static str, PartKey<'_, '_>); 12] = [
            ("the crate", PartKey { krate: CrateFacts { krate: ContentId([16; 32]), ..krate() }, ..base }),
            ("the file's entry facts", PartKey { file: facts(0, 12), ..base }),
            ("the file's index", PartKey { file: facts(1, 7), ..base }),
            ("a head's top entry", PartKey { heads: &top, ..base }),
            ("a head's want", PartKey { heads: &want, ..base }),
            ("a head's name", PartKey { heads: &named, ..base }),
            ("a head's member", PartKey { heads: &member, ..base }),
            ("a head's walk", PartKey { heads: &walked, ..base }),
            ("a head's macro", PartKey { heads: &by, ..base }),
            ("a call's row", PartKey { calls: &row, ..base }),
            ("a call's key", PartKey { calls: &call, ..base }),
            ("the heads", PartKey { heads: &[], ..base }),
        ];
        moved.iter().find(|(_, parts)| key_of(parts) == at).map(|(what, _)| *what)
    }

    /// Rule 21: both keys move with each input their record is a function of, a file's entry
    /// facts (what a body that comes to hold an item moves) and a call's directory row among
    /// them. PLANTED: a join key without the files' facts, and a part key without its calls'
    /// rows, are each caught (a listing stale for its key: a wrong answer, silently).
    #[test]
    fn the_keys_move_with_every_input() {
        assert_eq!(join_unmoved(&join_key), None, "every input moves a join's key");
        assert_eq!(part_unmoved(&part_key), None, "every input moves a part's key");
        let planted = |p: &JoinKey<'_>| join_key(&JoinKey { files: &[], ..*p });
        assert!(matches!(join_unmoved(&planted), Some("a file's entry facts")), "PLANTED: a join key without the files is caught");
        let planted = |p: &PartKey<'_, '_>| {
            let calls: [PlacedCall; 1] = [PlacedCall { top: 0, call: lib(0), row: ContentId([0; 32]) }];
            part_key(&PartKey { calls: &calls, ..*p })
        };
        assert_eq!(part_unmoved(&planted), Some("a call's row"), "PLANTED: a part key without its calls is caught");
    }
}
