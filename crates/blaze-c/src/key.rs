//! The import's keys and its read list (rules 21, 65, 77).
//!
//! - [`RequestKey`]: computed from TEXT before any header is read: the request's canonical form
//!   ([`crate::grammar::Request`], already normalized), the target's triple and C ABI facts, the
//!   dialect and the toolchain's digest. It finds the import's last read list.
//! - [`ReadList`]: every file the front end asked for, as the import machine answered it: each
//!   place with the digest of its bytes, or ABSENT (a probe in an earlier search folder that
//!   found nothing is a read too: a header that appears there later changes the result). Sorted,
//!   one entry a place. It is made only by the import ([`crate::import::Import`]) and handed back
//!   with the declarations (rule 76), so no caller keys a record by reads it did not see.
//! - [`ImportKey`]: the request key and the read list together: the declaration record's key.
//!   A record is valid by its import key, or else by its read list checked against today's files
//!   ([`ReadsView::first_change`]).
//!
//! What of a request changes the record, and so is in the request key: the headers in order
//! (each is included after the one before, so their order changes meaning), the include folders
//! in order (the search order), and the defines as a set sorted by name (every define is set
//! before the first header, so their order among themselves and among headers does not matter;
//! a name defined twice is refused by the grammar). What does not, and is left to
//! [`RenderKey`]: the module's name, the `allow` and `block` lists and `link`, which change the
//! rendering only, so two imports that differ in them share one declaration record.
//!
//! `inline_bodies` and `macro_fn` change the rendering AND ask the import's own C unit for code,
//! so they are in [`RenderKey`] and in [`UnitKey`], the unit's key, and never in the request
//! key: the declaration record is the same whatever bodies are asked for (the C compiler
//! expands a `macro_fn`'s macro in the unit; the record needs only that the macro exists).

use blaze_format_core::{FormatError, Own, Placed, Slice, Writer, WriterOpts, open_as, schema_of, Part, Record, SchemaHash};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::grammar::{MacroType, Request};
use crate::place::{Place, PlaceError, RelPath, Root};
use crate::{at, narrow};

/// A scalar's size and alignment in bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Scalar {
    pub size: u8,
    pub align: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Endian {
    Little = 0,
    Big = 1,
}

/// The target's C ABI facts the key holds: what changes a layout or a type the front end gives.
/// They are the toolchain's for the triple (the toolchain's digest covers how they were derived);
/// held in the key too so a request key says what it was computed for without the toolchain.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CAbi {
    pub endian: Endian,
    pub char_signed: bool,
    pub pointer: Scalar,
    pub bool_: Scalar,
    pub short: Scalar,
    pub int: Scalar,
    pub long: Scalar,
    pub long_long: Scalar,
    pub int128: Option<Scalar>,
    pub float: Scalar,
    pub double: Scalar,
    pub long_double: Scalar,
    /// `alignof(max_align_t)`.
    pub max_align: u8,
}

impl CAbi {
    /// The facts in the order the key hashes them.
    fn bytes(&self) -> [u8; 26] {
        let s = |s: Scalar| [s.size, s.align];
        let int128 = self.int128.map_or([0, 0], s);
        let mut out = [0u8; 26];
        let parts = [
            s(self.pointer),
            s(self.bool_),
            s(self.short),
            s(self.int),
            s(self.long),
            s(self.long_long),
            int128,
            s(self.float),
            s(self.double),
            s(self.long_double),
            [self.endian as u8, u8::from(self.char_signed)],
            [self.max_align, u8::from(self.int128.is_some())],
        ];
        for (k, pair) in parts.iter().enumerate() {
            out[2 * k] = pair[0];
            out[2 * k + 1] = pair[1];
        }
        out
    }
}

/// The target an import is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CTarget<'t> {
    /// The target triple, as the toolchain names it (`aarch64-apple-darwin`).
    pub triple: &'t str,
    pub abi: CAbi,
}

/// The C dialect the front end parses in. The import's grammar has no `std` argument yet; the
/// caller passes the toolchain's default for C imports, and the key holds it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CDialect {
    Gnu11 = 0,
    Gnu17 = 1,
    Gnu23 = 2,
}

/// What a request key is computed from: all of it text or facts known before the work.
#[derive(Clone, Copy, Debug)]
pub struct KeyInputs<'r> {
    pub request: &'r Request<'r>,
    pub target: CTarget<'r>,
    pub dialect: CDialect,
    /// The digest of the toolchain record the front end is (its clang, its shelf of headers).
    pub toolchain: ContentId,
}

/// The key of an import's declarations before its reads are known.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RequestKey(pub ContentId);

/// The declaration record's key: the request key and the read list.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ImportKey(pub ContentId);

/// The rendered module's key: the import key and what of the request only the rendering reads.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RenderKey(pub ContentId);

impl RequestKey {
    pub fn of(inputs: KeyInputs<'_>) -> RequestKey {
        let request = inputs.request;
        let mut h = Hasher::new("blaze-c request key");
        h.part(b"headers");
        h.part(&u64::try_from(request.headers.len()).expect("a count fits u64").to_le_bytes());
        for header in request.headers.iter() {
            h.part(header.as_str().as_bytes());
        }
        h.part(b"include");
        h.part(&u64::try_from(request.include.len()).expect("a count fits u64").to_le_bytes());
        for dir in request.include.iter() {
            h.part(dir.as_str().as_bytes());
        }
        h.part(b"define");
        h.part(&u64::try_from(request.defines.len()).expect("a count fits u64").to_le_bytes());
        for define in request.defines.iter() {
            h.part(define.name.as_bytes());
            match define.value {
                None => h.part(&[0]),
                Some(value) => h.part(&[1, u8::from(value.negative)]).part(value.text.as_bytes()),
            };
        }
        h.part(b"target");
        h.part(inputs.target.triple.as_bytes());
        h.part(&inputs.target.abi.bytes());
        h.part(&[inputs.dialect as u8]);
        h.id(&inputs.toolchain);
        RequestKey(h.finish())
    }
}

impl ImportKey {
    pub fn of(request: RequestKey, reads: &ReadList<'_>) -> ImportKey {
        let mut h = Hasher::new("blaze-c import key");
        h.id(&request.0);
        h.id(&reads.digest());
        ImportKey(h.finish())
    }
}

impl RenderKey {
    /// `unit`: the import's unit the rendering binds bodies and header-local variables to
    /// ([`UnitKey`]); `None` when there is none (each such declaration is then a finding).
    pub fn of(import: ImportKey, request: &Request<'_>, unit: Option<UnitKey>) -> RenderKey {
        let mut h = Hasher::new("blaze-c render key");
        h.id(&import.0);
        h.part(request.module.as_bytes());
        h.opt_part(request.link.map(str::as_bytes));
        for (label, list) in [(b"allow" as &[u8], &request.allow), (b"block", &request.block)] {
            h.part(label);
            h.part(&u64::try_from(list.len()).expect("a count fits u64").to_le_bytes());
            for pattern in list.iter() {
                h.part(pattern.text.as_bytes());
                h.part(&[u8::from(pattern.prefix)]);
            }
        }
        h.part(b"unit");
        h.opt_part(unit.as_ref().map(|u| u.0.0));
        unit_parts(&mut h, request);
        RenderKey(h.finish())
    }
}

/// The key of the import's own C unit: the code record the compile side makes for what the
/// headers define and the request asks for. It holds a thunk for each function the headers
/// define with internal or inline linkage (when `inline_bodies` is on), each header-local
/// variable and an accessor of its address, and a wrapper for each `macro_fn`, each exported
/// under [`UnitKey::symbol`].
///
/// Computed from what is known before any C is compiled (rule 65): the import key (the
/// declaration record, and so the headers' text and the defines), the SITE (the key of the
/// importing item, which the expansion side has: the `import!` call's item key) and what of
/// the request asks the unit for something (`inline_bodies`, the `macro_fn` signatures).
///
/// The declaration record does not depend on the unit, and its key does not change when the
/// unit is compiled: nothing the compile side makes is written into it. The rendering names the
/// unit's symbols by this key alone, so it neither waits for the code nor reads it; the code
/// record is found by this key when `run` needs the symbols.
///
/// Why the site is in the key: C gives each translation unit its own copy of a `static`
/// function and of a `static` variable. A Rust crate's `import!` is one translation unit, so
/// two imports of one header (two crates, or two modules of one) have two units and two
/// `PyArray_API` tables, as two C files including `__multiarray_api.h` do; one import used from
/// anywhere in its crate is one table, as one C file is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct UnitKey(pub ContentId);

/// How many bytes of the unit key a symbol carries: 64 bits, so two units in one program
/// collide with odds below one in 10^9 for a million imports.
const SYMBOL_KEY_BYTES: usize = 8;

impl UnitKey {
    pub fn of(import: ImportKey, site: ContentId, request: &Request<'_>) -> UnitKey {
        let mut h = Hasher::new("blaze-c unit key");
        h.id(&import.0);
        h.id(&site);
        unit_parts(&mut h, request);
        UnitKey(h.finish())
    }

    /// The symbol the unit exports for the header's `name` (a function's thunk, a header-local
    /// variable's address accessor, a `macro_fn`'s wrapper), into `out`:
    /// `__blaze_c_<16 hex digits of the key>_<name>`. Equal for the rendering and the compile
    /// side, which both compute it from the key; `__` begins a name C reserves, so no header's
    /// own symbol is one.
    pub fn symbol(&self, name: &str, out: &mut AString<'_>) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push_str("__blaze_c_");
        for byte in &self.0.0[..SYMBOL_KEY_BYTES] {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 15)]));
        }
        out.push('_');
        out.push_str(name);
    }
}

/// What of a request asks the unit for something: in the unit key and the render key.
fn unit_parts(h: &mut Hasher, request: &Request<'_>) {
    h.part(b"inline_bodies");
    h.part(&[u8::from(request.inline_bodies)]);
    h.part(b"macro_fn");
    h.part(&u64::try_from(request.macro_fns.len()).expect("a count fits u64").to_le_bytes());
    let ty = |h: &mut Hasher, t: &MacroType<'_>| {
        h.part(t.name.as_bytes());
        h.part(&[t.depth, t.muts]);
    };
    for f in request.macro_fns.iter() {
        h.part(f.name.as_bytes());
        let params = request.params_of(f);
        h.part(&u64::try_from(params.len()).expect("a count fits u64").to_le_bytes());
        for p in params {
            h.part(p.name.as_bytes());
            ty(h, &p.ty);
        }
        match &f.result {
            None => {
                h.part(&[0]);
            }
            Some(t) => {
                h.part(&[1]);
                ty(h, t);
            }
        }
    }
}

/// One read: a place and the digest of what was there, `None` for nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Read<'r> {
    pub place: Place<'r>,
    pub digest: Option<ContentId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    root: Root,
    start: u32,
    end: u32,
    digest: Option<ContentId>,
}

/// Why reads are not one read list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadsError {
    /// One place answered twice with different bytes (or once present, once absent) during one
    /// import: the file changed under it. The entry is by its index in the sorted list.
    Changed { entry: u32 },
    TooMany,
}

/// The reads of an import while it runs: in the order asked, possibly with repeats.
pub(crate) struct ReadsBuilder<'a> {
    text: AVec<'a, u8>,
    entries: AVec<'a, Entry>,
    alloc: Alloc<'a>,
}

impl<'a> ReadsBuilder<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> ReadsBuilder<'a> {
        ReadsBuilder { text: Vec::new_in(alloc), entries: Vec::new_in(alloc), alloc }
    }

    pub(crate) fn push(&mut self, place: Place<'_>, bytes: Option<&[u8]>) -> Result<(), ReadsError> {
        let start = narrow(self.text.len()).ok_or(ReadsError::TooMany)?;
        self.text.extend_from_slice(place.path.as_str().as_bytes());
        let end = narrow(self.text.len()).ok_or(ReadsError::TooMany)?;
        narrow(self.entries.len()).ok_or(ReadsError::TooMany)?;
        self.entries.push(Entry { root: place.root, start, end, digest: bytes.map(crate::model::content_digest) });
        Ok(())
    }

    /// The read list: sorted by place, one entry a place, refused when a place was answered two
    /// ways.
    pub(crate) fn seal(mut self) -> Result<ReadList<'a>, ReadsError> {
        let text = &self.text;
        let path = |e: &Entry| &text[at(e.start)..at(e.end)];
        self.entries.sort_unstable_by(|x, y| (x.root, path(x)).cmp(&(y.root, path(y))));
        let mut kept: AVec<'a, Entry> = Vec::new_in(self.alloc);
        for entry in self.entries.iter() {
            match kept.last() {
                Some(last) if last.root == entry.root && path(last) == path(entry) => {
                    if last.digest != entry.digest {
                        let entry = narrow(kept.len() - 1).ok_or(ReadsError::TooMany)?;
                        return Err(ReadsError::Changed { entry });
                    }
                }
                _ => kept.push(*entry),
            }
        }
        // The text of the kept entries alone, in their order, so equal lists are equal bytes.
        let mut packed: AVec<'a, u8> = Vec::new_in(self.alloc);
        for entry in kept.iter_mut() {
            let start = narrow(packed.len()).ok_or(ReadsError::TooMany)?;
            packed.extend_from_slice(&self.text[at(entry.start)..at(entry.end)]);
            entry.start = start;
            entry.end = narrow(packed.len()).ok_or(ReadsError::TooMany)?;
        }
        Ok(ReadList { text: packed, entries: kept })
    }
}

/// An import's read list: sorted by place, one entry a place (see the module's text).
pub struct ReadList<'a> {
    text: AVec<'a, u8>,
    entries: AVec<'a, Entry>,
}

impl<'a> ReadList<'a> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, k: usize) -> Option<Read<'_>> {
        let e = self.entries.get(k)?;
        let text = core::str::from_utf8(&self.text[at(e.start)..at(e.end)]).expect("pushed as a RelPath's text");
        let path = RelPath::new(text).expect("pushed as a RelPath");
        Some(Read { place: Place { root: e.root, path }, digest: e.digest })
    }

    pub fn iter(&self) -> impl Iterator<Item = Read<'_>> + '_ {
        (0..self.entries.len()).map(|k| self.get(k).expect("in range"))
    }

    /// The list's digest: what the import key holds of it.
    pub fn digest(&self) -> ContentId {
        let mut h = Hasher::new("blaze-c read list");
        for read in self.iter() {
            h.update(&[read.place.root.byte(), u8::from(read.digest.is_some())]);
            h.part(read.place.path.as_str().as_bytes());
            h.id(&read.digest.unwrap_or_default());
        }
        h.finish()
    }

    /// The list as a record (equal lists, equal bytes), read back by [`ReadsView::open`].
    pub fn encode<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, u8> {
        let mut out = Writer::new(WriterOpts { schema: reads_schema(), alloc: Own(alloc), capacity: None });
        let rows: AVec<'_, ReadRow> = {
            let mut rows = Vec::with_capacity_in(self.entries.len(), alloc);
            for e in self.entries.iter() {
                rows.push(ReadRow {
                    digest: e.digest.unwrap_or_default(),
                    start: e.start,
                    end: e.end,
                    root: e.root.byte(),
                    present: u8::from(e.digest.is_some()),
                    pad: [0; 2],
                });
            }
            rows
        };
        let text = Placed::push(&mut out, self.text.as_slice());
        let rows = Placed::push(&mut out, rows.as_slice());
        let root_at = out.next::<ReadsRoot>();
        let field = |offset: usize| root_at + u64::try_from(offset).expect("an offset fits u64");
        let root = ReadsRoot {
            text: text.slice(field(core::mem::offset_of!(ReadsRoot, text))),
            rows: rows.slice(field(core::mem::offset_of!(ReadsRoot, rows))),
        };
        let at = out.push(&root);
        out.finish(at)
    }
}

blaze_format_core::record! {
    /// One read of the record: the digest (zero when absent), the path's bytes in `text`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct ReadRow: 1 {
        pub digest: ContentId,
        pub start: u32,
        pub end: u32,
        pub root: u8,
        pub present: u8,
        pub pad: [u8; 2],
    }
}

blaze_format_core::record! {
    #[derive(Clone, Copy, Debug)]
    pub struct ReadsRoot: 1 {
        pub text: Slice<u8>,
        pub rows: Slice<ReadRow>,
    }
}

/// The read-list record's schema (D162: derived, never a version by hand).
pub fn reads_schema() -> SchemaHash {
    schema_of(&[Part::Str("blaze-c reads"), Part::Schema(<ReadsRoot as Record>::SCHEMA)])
}

/// Why bytes are not a read-list record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReadsOpenError {
    Format(FormatError),
    /// Row `row` is not a read: a path outside the text or not a [`RelPath`], a root or presence
    /// byte no value has, rows out of order or repeated.
    Invalid { row: u32, what: &'static str },
    Path { row: u32, error: PlaceError },
}

/// A read-list record, checked once by [`ReadsView::open`] and then read in place.
pub struct ReadsView<'b> {
    text: &'b [u8],
    rows: &'b [ReadRow],
}

impl<'b> ReadsView<'b> {
    pub fn open(buffer: &'b [u8]) -> Result<ReadsView<'b>, ReadsOpenError> {
        let root = open_as::<ReadsRoot>(buffer, reads_schema()).map_err(ReadsOpenError::Format)?;
        let text = root.text.follow(buffer).map_err(ReadsOpenError::Format)?;
        let rows = root.rows.follow(buffer).map_err(ReadsOpenError::Format)?;
        let view = ReadsView { text, rows };
        let mut last: Option<(Root, &str)> = None;
        for (k, row) in rows.iter().enumerate() {
            let row_no = narrow(k).ok_or(ReadsOpenError::Invalid { row: u32::MAX, what: "too many rows" })?;
            let bad = |what| ReadsOpenError::Invalid { row: row_no, what };
            let root = Root::from_byte(row.root).ok_or(bad("a root byte no root has"))?;
            if row.present > 1 || row.pad != [0; 2] || (row.present == 0 && row.digest != ContentId::default()) {
                return Err(bad("a presence byte, padding or absent digest that is not zero"));
            }
            let bytes = text.get(at(row.start)..at(row.end)).ok_or(bad("a path outside the text"))?;
            let path = core::str::from_utf8(bytes).map_err(|_| bad("a path that is not UTF-8"))?;
            RelPath::new(path).map_err(|error| ReadsOpenError::Path { row: row_no, error })?;
            if last.is_some_and(|l| l >= (root, path)) {
                return Err(bad("rows out of order or repeated"));
            }
            last = Some((root, path));
        }
        Ok(view)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn get(&self, k: usize) -> Option<Read<'b>> {
        let row = self.rows.get(k)?;
        let text = core::str::from_utf8(&self.text[at(row.start)..at(row.end)]).expect("checked by open");
        Some(Read {
            place: Place {
                root: Root::from_byte(row.root).expect("checked by open"),
                path: RelPath::new(text).expect("checked by open"),
            },
            digest: (row.present == 1).then_some(row.digest),
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = Read<'b>> + '_ {
        (0..self.rows.len()).map(|k| self.get(k).expect("in range"))
    }

    /// The first read whose place holds something else today (`today` gives a place's digest
    /// now, `None` for nothing there), or `None` when every read still holds: the record under
    /// this list's import key is then the import's answer, and no front end runs.
    pub fn first_change(&self, mut today: impl FnMut(Place<'b>) -> Option<ContentId>) -> Option<Read<'b>> {
        self.iter().find(|read| today(read.place) != read.digest)
    }
}
