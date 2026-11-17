//! The record of one index file: its versions as fixed rows, sorted, its strings in one buffer,
//! and the validators its answer came with. The parser ([`super::line`]) writes it, the cache
//! keeps it as written (one file under the directory grant, [`super::request::cache_path_in`]),
//! and [`IndexView`] reads it in place: every offset, range and code is checked once, when it
//! is opened, and never on a read after.
//!
//! The form, every number little-endian:
//!
//! ```text
//! header   64 bytes: MAGIC (7) FORMAT (1), counts of versions, deps, features and values and
//!          the strings' length (u32 each), the validator's kind (u8, 3 zero bytes), the
//!          validator and the file's name as spans, the record's whole length (u64), 8 zero
//! versions VERSION_ROW bytes each, sorted by semver precedence, then by the version as
//!          written, then by line
//! deps     DEP_ROW bytes each, every version's in one run
//! features FEATURE_ROW bytes each, `features` and `features2` in the line's order: a name may
//!          come twice, and its values are both rows' together, as cargo adds them
//! values   8 bytes each: a span, the strings of a dependency's or a feature's list
//! strings  UTF-8, every span's text
//! ```
//!
//! A span is an offset and a length into the strings; `u32::MAX` as the offset is "none".

#![blaze::sans_io]

use blaze_json::Str;
use blaze_own::{Alloc, String, Vec};
use blaze_text::{Put, Show, Writes as _};

use super::line::Field;

/// The first seven bytes of every index record.
pub const MAGIC: [u8; 7] = *b"blzidx\0";
/// The form this crate writes; a record of another is [`ViewRefused::Old`], rebuilt.
pub const FORMAT: u8 = 1;
pub const HEADER: usize = 64;
pub const VERSION_ROW: usize = 104;
pub const DEP_ROW: usize = 56;
pub const FEATURE_ROW: usize = 16;
pub const VALUE: usize = 8;

const NONE_AT: u32 = u32::MAX;

const STATUS_CANDIDATE: u8 = 0;
const STATUS_YANKED: u8 = 1;
const STATUS_UNSUPPORTED: u8 = 2;
const STATUS_INVALID: u8 = 3;

const VALIDATOR_NONE: u8 = 0;
const VALIDATOR_ETAG: u8 = 1;
const VALIDATOR_LAST_MODIFIED: u8 = 2;

pub(crate) const FLAG_OPTIONAL: u8 = 1;
pub(crate) const FLAG_DEFAULT_FEATURES: u8 = 2;
pub(crate) const FLAG_PUBLIC: u8 = 4;
pub(crate) const FLAG_ARTIFACT: u8 = 8;
const FLAGS_ALL: u8 = FLAG_OPTIONAL | FLAG_DEFAULT_FEATURES | FLAG_PUBLIC | FLAG_ARTIFACT;

/// Text in the record's strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Span {
    pub(crate) at: u32,
    pub(crate) len: u32,
}

impl Span {
    pub(crate) const NONE: Span = Span { at: NONE_AT, len: 0 };

    pub(crate) fn is_none(self) -> bool {
        self.at == NONE_AT
    }
}

/// A run of rows: the first and how many.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Run {
    pub(crate) first: u32,
    pub(crate) count: u32,
}

impl Run {
    pub(crate) const EMPTY: Run = Run { first: 0, count: 0 };
}

/// What the registry's answer said its file is, to ask with next time: cargo's
/// `index_version` (an `ETag` before a `Last-Modified`, as cargo prefers).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Validator<'r> {
    /// The answer had neither: the next request is unconditional.
    None,
    Etag(&'r str),
    LastModified(&'r str),
}

/// What cargo does with a version (blaze-resolve's `IndexSummary` variants, by name).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Candidate,
    Yanked,
    /// Its line is of a schema (`v`) newer than [`super::line::INDEX_V_MAX`].
    Unsupported(u32),
    /// Its line names its crate and version, and `field` is not what an index line holds:
    /// the version is known, never chosen (cargo's `IndexSummary::Invalid`).
    Invalid(Field),
}

/// How a dependency is used (the line's `kind`; an unknown word is normal, as cargo reads it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DepKind {
    Normal,
    Development,
    Build,
}

impl DepKind {
    pub(crate) fn code(self) -> u8 {
        match self {
            DepKind::Normal => 0,
            DepKind::Development => 1,
            DepKind::Build => 2,
        }
    }

    fn from_code(code: u8) -> Option<DepKind> {
        match code {
            0 => Some(DepKind::Normal),
            1 => Some(DepKind::Development),
            2 => Some(DepKind::Build),
            _ => None,
        }
    }
}

/// One version as the parser holds it before the record is written.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VRow {
    pub(crate) name: Span,
    pub(crate) vers: Span,
    pub(crate) cksum: [u8; 32],
    pub(crate) links: Span,
    pub(crate) rust_version: Span,
    pub(crate) pubtime: Span,
    pub(crate) deps: Run,
    pub(crate) features: Run,
    pub(crate) line: u32,
    pub(crate) v: u32,
    pub(crate) status: Status,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DRow {
    pub(crate) name: Span,
    pub(crate) package: Span,
    pub(crate) req: Span,
    pub(crate) target: Span,
    pub(crate) registry: Span,
    pub(crate) features: Run,
    pub(crate) kind: DepKind,
    pub(crate) flags: u8,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FRow {
    pub(crate) name: Span,
    pub(crate) values: Run,
}

/// A file's rows as the parser builds them, every string in one buffer.
pub(crate) struct Table<'a> {
    pub(crate) strings: String<Alloc<'a>>,
    pub(crate) versions: Vec<VRow, Alloc<'a>>,
    pub(crate) deps: Vec<DRow, Alloc<'a>>,
    pub(crate) features: Vec<FRow, Alloc<'a>>,
    pub(crate) values: Vec<Span, Alloc<'a>>,
}

/// The record would pass `u32` offsets: more text than a record's spans address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TooLarge {
    pub(crate) len: u64,
}

impl<'a> Table<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Table<'a> {
        Table {
            strings: String::new_in(alloc),
            versions: Vec::new_in(alloc),
            deps: Vec::new_in(alloc),
            features: Vec::new_in(alloc),
            values: Vec::new_in(alloc),
        }
    }

    /// `text` decoded into the strings.
    pub(crate) fn text(&mut self, text: &Str<'_>) -> Span {
        let at = self.strings.len();
        text.decode_into(&mut self.strings);
        self.span(at)
    }

    /// `text` as it is, into the strings.
    pub(crate) fn plain(&mut self, text: &str) -> Span {
        let at = self.strings.len();
        self.strings.push_str(text);
        self.span(at)
    }

    /// The span from `at` to the strings' end. The parser holds a file to `u32::MAX` bytes and
    /// decoding never lengthens a string, so every offset fits; the record's own check is
    /// [`Table::into_record_in`]'s.
    fn span(&self, at: usize) -> Span {
        let to_u32 = |n: usize| u32::try_from(n).unwrap_or(NONE_AT - 1);
        Span { at: to_u32(at), len: to_u32(self.strings.len() - at) }
    }

    pub(crate) fn str(&self, span: Span) -> &str {
        &self.strings.as_str()[span.at as usize..(span.at + span.len) as usize]
    }

    /// The record: the rows in `order` (indices into `versions`), `name` the file's and
    /// `validator` its answer's.
    pub(crate) fn into_record_in(
        mut self,
        order: &[u32],
        name: &str,
        validator: Validator<'_>,
        alloc: Alloc<'a>,
    ) -> Result<Vec<u8, Alloc<'a>>, TooLarge> {
        let name = self.plain(name);
        let (kind, validator) = match validator {
            Validator::None => (VALIDATOR_NONE, Span::NONE),
            Validator::Etag(text) => (VALIDATOR_ETAG, self.plain(text)),
            Validator::LastModified(text) => (VALIDATOR_LAST_MODIFIED, self.plain(text)),
        };
        let total = HEADER as u64
            + (order.len() * VERSION_ROW) as u64
            + (self.deps.len() * DEP_ROW) as u64
            + (self.features.len() * FEATURE_ROW) as u64
            + (self.values.len() * VALUE) as u64
            + self.strings.len() as u64;
        if self.strings.len() as u64 >= u64::from(NONE_AT) || total > u64::from(u32::MAX) {
            return Err(TooLarge { len: total });
        }
        let mut out = Vec::with_capacity_in(total as usize, alloc);
        out.extend_from_slice(&MAGIC);
        out.push(FORMAT);
        for count in [order.len(), self.deps.len(), self.features.len(), self.values.len(), self.strings.len()] {
            put32(&mut out, count as u32);
        }
        out.extend_from_slice(&[kind, 0, 0, 0]);
        put_span(&mut out, validator);
        put_span(&mut out, name);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        for at in order {
            let row = &self.versions[*at as usize];
            put_span(&mut out, row.name);
            put_span(&mut out, row.vers);
            out.extend_from_slice(&row.cksum);
            put_span(&mut out, row.links);
            put_span(&mut out, row.rust_version);
            put_span(&mut out, row.pubtime);
            put_run(&mut out, row.deps);
            put_run(&mut out, row.features);
            put32(&mut out, row.line);
            put32(&mut out, row.v);
            let (status, field) = match row.status {
                Status::Candidate => (STATUS_CANDIDATE, 0),
                Status::Yanked => (STATUS_YANKED, 0),
                Status::Unsupported(_) => (STATUS_UNSUPPORTED, 0),
                Status::Invalid(field) => (STATUS_INVALID, field.code()),
            };
            out.extend_from_slice(&[status, field, 0, 0, 0, 0, 0, 0]);
        }
        for dep in self.deps.as_slice() {
            for span in [dep.name, dep.package, dep.req, dep.target, dep.registry] {
                put_span(&mut out, span);
            }
            put_run(&mut out, dep.features);
            out.extend_from_slice(&[dep.kind.code(), dep.flags, 0, 0, 0, 0, 0, 0]);
        }
        for feature in self.features.as_slice() {
            put_span(&mut out, feature.name);
            put_run(&mut out, feature.values);
        }
        for value in self.values.as_slice() {
            put_span(&mut out, *value);
        }
        out.extend_from_slice(self.strings.as_bytes());
        debug_assert_eq!(out.len() as u64, total);
        Ok(out)
    }
}

fn put32(out: &mut Vec<u8, Alloc<'_>>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_span(out: &mut Vec<u8, Alloc<'_>>, span: Span) {
    put32(out, span.at);
    put32(out, span.len);
}

fn put_run(out: &mut Vec<u8, Alloc<'_>>, run: Run) {
    put32(out, run.first);
    put32(out, run.count);
}

/// Why bytes are not an index record this crate reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewRefused {
    /// Shorter than its header, or than the header says.
    Short { len: u64, wanted: u64 },
    /// Not an index record at all.
    Magic,
    /// An index record of another form: an old home's, reported and rebuilt.
    Old { format: u8 },
    /// Its header's length is not its length.
    Length { said: u64, len: u64 },
    /// A span past the strings, or not on a character's edge.
    Span { at: u64 },
    /// A run of rows past its table.
    Run { at: u64 },
    /// The strings are not UTF-8.
    Utf8,
    /// A status, dependency kind, flag or field code no writer writes.
    Code { at: u64 },
    /// A record of another file than the one it is kept for.
    OtherFile,
}

impl Show for ViewRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match *self {
            ViewRefused::Short { len, wanted } => out.str("an index record of ").int(len).str(" bytes, short of ").int(wanted),
            ViewRefused::Magic => out.str("not an index record"),
            ViewRefused::Old { format } => out.str("an index record of form ").int(format).str(", not ").int(FORMAT),
            ViewRefused::Length { said, len } => {
                out.str("an index record of ").int(len).str(" bytes whose header says ").int(said)
            }
            ViewRefused::Span { at } => out.str("an index record with a span off its strings, at byte ").int(at),
            ViewRefused::Run { at } => out.str("an index record with a run of rows off its table, at byte ").int(at),
            ViewRefused::Utf8 => out.str("an index record whose strings are not UTF-8"),
            ViewRefused::Code { at } => out.str("an index record with a code no writer writes, at byte ").int(at),
            ViewRefused::OtherFile => out.str("an index record of another crate's file"),
        };
    }
}

/// An index record, read in place: see the module.
#[derive(Clone, Copy, Debug)]
pub struct IndexView<'r> {
    bytes: &'r [u8],
    strings: &'r str,
    versions: u32,
    deps: u32,
    features: u32,
    values: u32,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

impl<'r> IndexView<'r> {
    /// `bytes` as a record, every offset, run and code in it checked now.
    pub fn open(bytes: &'r [u8]) -> Result<IndexView<'r>, ViewRefused> {
        let len = bytes.len() as u64;
        if bytes.len() < HEADER {
            return Err(ViewRefused::Short { len, wanted: HEADER as u64 });
        }
        if bytes[..7] != MAGIC {
            return Err(ViewRefused::Magic);
        }
        if bytes[7] != FORMAT {
            return Err(ViewRefused::Old { format: bytes[7] });
        }
        let (versions, deps, features, values, strings) =
            (u32_at(bytes, 8), u32_at(bytes, 12), u32_at(bytes, 16), u32_at(bytes, 20), u32_at(bytes, 24));
        let mut said = [0; 8];
        said.copy_from_slice(&bytes[48..56]);
        let said = u64::from_le_bytes(said);
        let wanted = HEADER as u64
            + u64::from(versions) * VERSION_ROW as u64
            + u64::from(deps) * DEP_ROW as u64
            + u64::from(features) * FEATURE_ROW as u64
            + u64::from(values) * VALUE as u64
            + u64::from(strings);
        if said != len {
            return Err(ViewRefused::Length { said, len });
        }
        if wanted != len {
            return Err(ViewRefused::Short { len, wanted });
        }
        let Ok(text) = core::str::from_utf8(&bytes[(len - u64::from(strings)) as usize..]) else {
            return Err(ViewRefused::Utf8);
        };
        let view = IndexView { bytes, strings: text, versions, deps, features, values };
        view.check()?;
        Ok(view)
    }

    /// Every span, run and code, once.
    fn check(&self) -> Result<(), ViewRefused> {
        let kind = self.bytes[28];
        if kind > VALIDATOR_LAST_MODIFIED || self.bytes[29..32] != [0, 0, 0] {
            return Err(ViewRefused::Code { at: 28 });
        }
        self.check_span(32, kind != VALIDATOR_NONE)?;
        self.check_span(40, true)?;
        for row in 0..self.versions as usize {
            let at = self.versions_at() + row * VERSION_ROW;
            self.check_span(at, true)?;
            self.check_span(at + 8, true)?;
            for field in [48, 56, 64] {
                self.check_span(at + field, false)?;
            }
            self.check_run(at + 72, self.deps)?;
            self.check_run(at + 80, self.features)?;
            let (status, field) = (self.bytes[at + 96], self.bytes[at + 97]);
            let known = match status {
                STATUS_CANDIDATE | STATUS_YANKED | STATUS_UNSUPPORTED => field == 0,
                STATUS_INVALID => Field::from_code(field).is_some(),
                _ => false,
            };
            if !known || self.bytes[at + 98..at + VERSION_ROW] != [0; 6] {
                return Err(ViewRefused::Code { at: (at + 96) as u64 });
            }
        }
        for row in 0..self.deps as usize {
            let at = self.deps_at() + row * DEP_ROW;
            self.check_span(at, true)?;
            for field in [8, 24, 32] {
                self.check_span(at + field, false)?;
            }
            self.check_span(at + 16, true)?;
            self.check_run(at + 40, self.values)?;
            let (kind, flags) = (self.bytes[at + 48], self.bytes[at + 49]);
            if DepKind::from_code(kind).is_none() || flags & !FLAGS_ALL != 0 || self.bytes[at + 50..at + DEP_ROW] != [0; 6] {
                return Err(ViewRefused::Code { at: (at + 48) as u64 });
            }
        }
        for row in 0..self.features as usize {
            let at = self.features_at() + row * FEATURE_ROW;
            self.check_span(at, true)?;
            self.check_run(at + 8, self.values)?;
        }
        for row in 0..self.values as usize {
            self.check_span(self.values_at() + row * VALUE, true)?;
        }
        Ok(())
    }

    fn check_span(&self, at: usize, required: bool) -> Result<(), ViewRefused> {
        let (start, len) = (u32_at(self.bytes, at), u32_at(self.bytes, at + 4));
        if start == NONE_AT && len == 0 && !required {
            return Ok(());
        }
        let end = u64::from(start) + u64::from(len);
        let fits = end <= self.strings.len() as u64
            && self.strings.is_char_boundary(start as usize)
            && self.strings.is_char_boundary(end as usize);
        if !fits {
            return Err(ViewRefused::Span { at: at as u64 });
        }
        Ok(())
    }

    fn check_run(&self, at: usize, rows: u32) -> Result<(), ViewRefused> {
        let (first, count) = (u32_at(self.bytes, at), u32_at(self.bytes, at + 4));
        if u64::from(first) + u64::from(count) > u64::from(rows) {
            return Err(ViewRefused::Run { at: at as u64 });
        }
        Ok(())
    }

    fn versions_at(&self) -> usize {
        HEADER
    }

    fn deps_at(&self) -> usize {
        self.versions_at() + self.versions as usize * VERSION_ROW
    }

    fn features_at(&self) -> usize {
        self.deps_at() + self.deps as usize * DEP_ROW
    }

    fn values_at(&self) -> usize {
        self.features_at() + self.features as usize * FEATURE_ROW
    }

    /// The span at `at`'s text: checked at open.
    fn text(&self, at: usize) -> &'r str {
        let start = u32_at(self.bytes, at) as usize;
        &self.strings[start..start + u32_at(self.bytes, at + 4) as usize]
    }

    fn opt_text(&self, at: usize) -> Option<&'r str> {
        (u32_at(self.bytes, at) != NONE_AT).then(|| self.text(at))
    }

    /// The file's name as it was asked for (lower case).
    pub fn name(&self) -> &'r str {
        self.text(40)
    }

    pub fn validator(&self) -> Validator<'r> {
        match self.bytes[28] {
            VALIDATOR_ETAG => Validator::Etag(self.text(32)),
            VALIDATOR_LAST_MODIFIED => Validator::LastModified(self.text(32)),
            _ => Validator::None,
        }
    }

    /// How many versions the file has.
    pub fn len(&self) -> usize {
        self.versions as usize
    }

    pub fn is_empty(&self) -> bool {
        self.versions == 0
    }

    /// Version `at`, in precedence order (`at < len`).
    pub fn version(&self, at: usize) -> VersionRow<'r> {
        assert!(at < self.len(), "version {at} of an index record of {}", self.len());
        VersionRow { view: *self, at: self.versions_at() + at * VERSION_ROW }
    }

    /// Every version, lowest precedence first.
    pub fn versions(&self) -> impl Iterator<Item = VersionRow<'r>> + 'r {
        let view = *self;
        (0..view.len()).map(move |at| view.version(at))
    }

    /// The bytes it was opened over: what the cache keeps.
    pub fn bytes(&self) -> &'r [u8] {
        self.bytes
    }
}

/// One version of an index record.
#[derive(Clone, Copy, Debug)]
pub struct VersionRow<'r> {
    view: IndexView<'r>,
    at: usize,
}

impl<'r> VersionRow<'r> {
    /// The crate's name as this line writes it (its case is the publisher's).
    pub fn name(&self) -> &'r str {
        self.view.text(self.at)
    }

    /// The version as written (`vers`); it parses by `blaze_semver::Limits::CARGO`.
    pub fn vers(&self) -> &'r str {
        self.view.text(self.at + 8)
    }

    /// The `.crate`'s SHA-256 (`cksum`); zeros for an [`Status::Invalid`] version without one.
    pub fn cksum(&self) -> [u8; 32] {
        let mut sum = [0; 32];
        sum.copy_from_slice(&self.view.bytes[self.at + 16..self.at + 48]);
        sum
    }

    pub fn links(&self) -> Option<&'r str> {
        self.view.opt_text(self.at + 48)
    }

    /// `rust_version` as written; the resolver parses it.
    pub fn rust_version(&self) -> Option<&'r str> {
        self.view.opt_text(self.at + 56)
    }

    /// `pubtime`, checked to be cargo's `yyyy-mm-ddThh:mm:ssZ`.
    pub fn pubtime(&self) -> Option<&'r str> {
        self.view.opt_text(self.at + 64)
    }

    /// The line of the file it was read from, from 1.
    pub fn line(&self) -> u32 {
        u32_at(self.view.bytes, self.at + 88)
    }

    /// Its schema (`v`), 1 when the line has none.
    pub fn schema(&self) -> u32 {
        u32_at(self.view.bytes, self.at + 92)
    }

    pub fn status(&self) -> Status {
        match self.view.bytes[self.at + 96] {
            STATUS_YANKED => Status::Yanked,
            STATUS_UNSUPPORTED => Status::Unsupported(self.schema()),
            STATUS_INVALID => {
                Status::Invalid(Field::from_code(self.view.bytes[self.at + 97]).unwrap_or_else(|| unreachable!("checked at open")))
            }
            _ => Status::Candidate,
        }
    }

    /// Its dependencies, in the line's order (none for an [`Status::Invalid`] version).
    pub fn deps(&self) -> impl Iterator<Item = DepRow<'r>> + 'r {
        let view = self.view;
        let (first, count) = (u32_at(view.bytes, self.at + 72) as usize, u32_at(view.bytes, self.at + 76) as usize);
        (first..first + count).map(move |row| DepRow { view, at: view.deps_at() + row * DEP_ROW })
    }

    /// Its features, `features` and `features2` in the line's order (a name may come twice:
    /// see the module).
    pub fn features(&self) -> impl Iterator<Item = FeatureRow<'r>> + 'r {
        let view = self.view;
        let (first, count) = (u32_at(view.bytes, self.at + 80) as usize, u32_at(view.bytes, self.at + 84) as usize);
        (first..first + count).map(move |row| FeatureRow { view, at: view.features_at() + row * FEATURE_ROW })
    }
}

/// The values of a run at `at`: a dependency's features or a feature's list.
fn values_of<'r>(view: IndexView<'r>, at: usize) -> impl Iterator<Item = &'r str> + 'r {
    let (first, count) = (u32_at(view.bytes, at) as usize, u32_at(view.bytes, at + 4) as usize);
    (first..first + count).map(move |row| view.text(view.values_at() + row * VALUE))
}

/// One dependency of a version.
#[derive(Clone, Copy, Debug)]
pub struct DepRow<'r> {
    view: IndexView<'r>,
    at: usize,
}

impl<'r> DepRow<'r> {
    /// The name the depending crate knows it by (the line's `name`).
    pub fn name_in_toml(&self) -> &'r str {
        self.view.text(self.at)
    }

    /// The crate's own name: `package` when the line renames it, else `name`.
    pub fn package(&self) -> &'r str {
        self.view.opt_text(self.at + 8).unwrap_or_else(|| self.name_in_toml())
    }

    /// The requirement as written (`req`); it parses by `blaze_semver::Limits::CARGO`.
    pub fn req(&self) -> &'r str {
        self.view.text(self.at + 16)
    }

    /// `target`: a `cfg(..)` or a target name, as written; the resolver parses it.
    pub fn target(&self) -> Option<&'r str> {
        self.view.opt_text(self.at + 24)
    }

    /// `registry`: the URL of the registry it is from, when not this one.
    pub fn registry(&self) -> Option<&'r str> {
        self.view.opt_text(self.at + 32)
    }

    /// Its features, the empty ones older registries wrote left out, as cargo leaves them.
    pub fn features(&self) -> impl Iterator<Item = &'r str> + 'r {
        values_of(self.view, self.at + 40)
    }

    pub fn kind(&self) -> DepKind {
        DepKind::from_code(self.view.bytes[self.at + 48]).unwrap_or_else(|| unreachable!("checked at open"))
    }

    fn flag(&self, flag: u8) -> bool {
        self.view.bytes[self.at + 49] & flag != 0
    }

    pub fn optional(&self) -> bool {
        self.flag(FLAG_OPTIONAL)
    }

    pub fn default_features(&self) -> bool {
        self.flag(FLAG_DEFAULT_FEATURES)
    }

    pub fn public(&self) -> bool {
        self.flag(FLAG_PUBLIC)
    }

    /// The line names an artifact (`-Zbindeps`): blaze does not read those, and the resolver
    /// makes the version invalid, as blaze-units did.
    pub fn artifact(&self) -> bool {
        self.flag(FLAG_ARTIFACT)
    }
}

/// One feature of a version and what it enables.
#[derive(Clone, Copy, Debug)]
pub struct FeatureRow<'r> {
    view: IndexView<'r>,
    at: usize,
}

impl<'r> FeatureRow<'r> {
    pub fn name(&self) -> &'r str {
        self.view.text(self.at)
    }

    pub fn values(&self) -> impl Iterator<Item = &'r str> + 'r {
        values_of(self.view, self.at + 8)
    }
}
