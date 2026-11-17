//! One index file parsed into its record ([`super::record`]): cargo 1.99's format, a JSON
//! object a line, one per published version (`name`, `vers`, `deps`, `cksum`, `features`,
//! `features2`, `yanked`, `links`, `rust_version`, `pubtime`, `v`; each dependency `name`,
//! `req`, `features`, `optional`, `default_features`, `target`, `kind`, `registry`, `package`,
//! `public`, `artifact`), read with blaze-json's [`Reader`] (no tree is built) and versions and
//! requirements with blaze-semver at cargo's limits (`Limits::CARGO`, the parser blaze-resolve's
//! `Dependency` uses).
//!
//! What cargo does with a line is kept, and nothing is let pass in silence:
//!
//! - a line that is not JSON (a torn file), not an object, without a `name` or `vers`, with a
//!   `vers` that is no version, of another crate, or with a key twice: the file is refused, by
//!   [`IndexRefused`] naming the line and the field (cargo drops such a line; blaze names it);
//! - a line whose other fields are not what an index line holds (a dependency's `req` no
//!   requirement, a `cksum` not 64 hex digits, a field of the wrong type, `deps` or `cksum`
//!   missing): the version is kept as [`Status::Invalid`] naming the field, with no
//!   dependencies or features, as cargo keeps it (`IndexSummary::Invalid`);
//! - a line of a schema past [`INDEX_V_MAX`]: [`Status::Unsupported`];
//! - keys the format gains later are let be: the registry's format grows, and a reader must
//!   tolerate it.

#![blaze::sans_io]

use blaze_json::{Event, Expected, Reader, Str, What};
use blaze_own::{Alloc, String, Vec};
use blaze_text::{Put, PutDisplay as _, Show, Writes as _};

use super::record::{
    DRow, DepKind, FLAG_ARTIFACT, FLAG_DEFAULT_FEATURES, FLAG_OPTIONAL, FLAG_PUBLIC, FRow, Run, Span, Status, Table, VRow,
    Validator,
};

/// The newest schema (`v`) of an index line cargo 1.99 reads without `-Zbindeps`.
pub const INDEX_V_MAX: u32 = 2;

/// Every bound on one index file (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexLimits {
    /// Bytes of one file; at most `u32::MAX`, so every offset of its record fits.
    pub most_bytes: u64,
    /// Lines of one file.
    pub most_lines: u32,
}

impl IndexLimits {
    /// 32 MiB and 256 Ki lines: the largest crates.io files (a few thousand versions) are a
    /// few MiB (blaze-audit's `MOST_INDEX_BYTES` is the same bound).
    pub const REAL: IndexLimits = IndexLimits { most_bytes: 32 << 20, most_lines: 1 << 18 };
}

/// A line's JSON: no line nests deeper than a dependency's feature list.
const LINE_LIMITS: blaze_json::Limits = blaze_json::Limits {
    most_bytes: usize::MAX,
    most_depth: 16,
    most_number_len: 40,
    duplicates: blaze_json::Duplicates::Refuse,
};

/// A field of an index line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    /// The line itself: its JSON, or its shape.
    Json,
    Name,
    Vers,
    Deps,
    Cksum,
    Features,
    Features2,
    Yanked,
    Links,
    RustVersion,
    Pubtime,
    V,
    /// An element of `deps` that is not an object.
    Dep,
    DepName,
    DepReq,
    DepFeatures,
    DepOptional,
    DepDefaultFeatures,
    DepTarget,
    DepKind,
    DepRegistry,
    DepPackage,
    DepPublic,
    DepArtifact,
    /// The file as a whole (its size, its lines).
    File,
}

const FIELDS: [Field; 25] = [
    Field::Json,
    Field::Name,
    Field::Vers,
    Field::Deps,
    Field::Cksum,
    Field::Features,
    Field::Features2,
    Field::Yanked,
    Field::Links,
    Field::RustVersion,
    Field::Pubtime,
    Field::V,
    Field::Dep,
    Field::DepName,
    Field::DepReq,
    Field::DepFeatures,
    Field::DepOptional,
    Field::DepDefaultFeatures,
    Field::DepTarget,
    Field::DepKind,
    Field::DepRegistry,
    Field::DepPackage,
    Field::DepPublic,
    Field::DepArtifact,
    Field::File,
];

impl Field {
    /// Its code in a record: from 1 (0 is "no field").
    pub(crate) fn code(self) -> u8 {
        let at = FIELDS.iter().position(|it| *it == self).unwrap_or_else(|| unreachable!("every field is listed"));
        (at + 1) as u8
    }

    pub(crate) fn from_code(code: u8) -> Option<Field> {
        FIELDS.get(usize::from(code).checked_sub(1)?).copied()
    }

    /// As the format names it.
    pub fn name(self) -> &'static str {
        match self {
            Field::Json => "the line",
            Field::Name => "name",
            Field::Vers => "vers",
            Field::Deps => "deps",
            Field::Cksum => "cksum",
            Field::Features => "features",
            Field::Features2 => "features2",
            Field::Yanked => "yanked",
            Field::Links => "links",
            Field::RustVersion => "rust_version",
            Field::Pubtime => "pubtime",
            Field::V => "v",
            Field::Dep => "deps[]",
            Field::DepName => "deps[].name",
            Field::DepReq => "deps[].req",
            Field::DepFeatures => "deps[].features",
            Field::DepOptional => "deps[].optional",
            Field::DepDefaultFeatures => "deps[].default_features",
            Field::DepTarget => "deps[].target",
            Field::DepKind => "deps[].kind",
            Field::DepRegistry => "deps[].registry",
            Field::DepPackage => "deps[].package",
            Field::DepPublic => "deps[].public",
            Field::DepArtifact => "deps[].artifact",
            Field::File => "the file",
        }
    }

    fn top(word: &str) -> Option<Field> {
        Some(match word {
            "name" => Field::Name,
            "vers" => Field::Vers,
            "deps" => Field::Deps,
            "cksum" => Field::Cksum,
            "features" => Field::Features,
            "features2" => Field::Features2,
            "yanked" => Field::Yanked,
            "links" => Field::Links,
            "rust_version" => Field::RustVersion,
            "pubtime" => Field::Pubtime,
            "v" => Field::V,
            _ => return None,
        })
    }

    fn dep(word: &str) -> Option<Field> {
        Some(match word {
            "name" => Field::DepName,
            "req" => Field::DepReq,
            "features" => Field::DepFeatures,
            "optional" => Field::DepOptional,
            "default_features" => Field::DepDefaultFeatures,
            "target" => Field::DepTarget,
            "kind" => Field::DepKind,
            "registry" => Field::DepRegistry,
            "package" => Field::DepPackage,
            "public" => Field::DepPublic,
            "artifact" => Field::DepArtifact,
            _ => return None,
        })
    }

    fn bit(self) -> u32 {
        1 << self.code()
    }
}

/// What is wrong with a field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    /// Not JSON, at this column (in bytes, from 1).
    Json { what: What, column: u32 },
    NotAnObject,
    Missing,
    /// The key comes twice in one object.
    Twice,
    WrongType,
    NotAVersion,
    /// The line is of another crate than the file's.
    OtherCrate,
    TooLarge { len: u64, most: u64 },
    TooManyLines { most: u32 },
}

/// Why an index file was refused: the line (from 1; 0 for the file as a whole), the field and
/// what is wrong with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexRefused {
    pub line: u32,
    pub field: Field,
    pub bad: Bad,
}

impl Show for IndexRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        if self.line != 0 {
            out.str("line ").int(self.line).str(", ");
        }
        out.str("`").str(self.field.name()).str("`: ");
        match self.bad {
            Bad::Json { what, column } => out.str("not JSON at column ").int(column).str(": ").put_display(&what),
            Bad::NotAnObject => out.str("not a JSON object"),
            Bad::Missing => out.str("missing"),
            Bad::Twice => out.str("given twice"),
            Bad::WrongType => out.str("not of its type"),
            Bad::NotAVersion => out.str("not a version"),
            Bad::OtherCrate => out.str("of another crate than its file's"),
            Bad::TooLarge { len, most } => out.str("of ").byte_count(len).str(", past its limit of ").byte_count(most),
            Bad::TooManyLines { most } => out.str("more than ").int(most).str(" lines"),
        };
    }
}

fn refused(line: u32, field: Field, bad: Bad) -> IndexRefused {
    IndexRefused { line, field, bad }
}

fn json(line: u32, error: blaze_json::Error) -> IndexRefused {
    let column = u32::try_from(error.at).unwrap_or(u32::MAX - 1) + 1;
    refused(line, Field::Json, Bad::Json { what: error.what, column })
}

/// The index file `bytes` of the crate `name` (the file's name: lower case, as asked) parsed
/// into its record, `validator` the answer's, every buffer in `alloc`: see the module.
pub fn parse_in<'a>(
    name: &str,
    bytes: &[u8],
    validator: Validator<'_>,
    limits: IndexLimits,
    alloc: Alloc<'a>,
) -> Result<Vec<u8, Alloc<'a>>, IndexRefused> {
    let most = limits.most_bytes.min(u64::from(u32::MAX));
    if bytes.len() as u64 > most {
        return Err(refused(0, Field::File, Bad::TooLarge { len: bytes.len() as u64, most }));
    }
    let mut table = Table::new_in(alloc);
    let mut parsed: Vec<(blaze_semver::Version<Alloc<'a>>, u32), Alloc<'a>> = Vec::new_in(alloc);
    let mut scratch = String::new_in(alloc);
    let mut no = 0u32;
    for line in bytes.split(|it| *it == b'\n') {
        no += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if parsed.len() as u64 >= u64::from(limits.most_lines) {
            return Err(refused(no, Field::File, Bad::TooManyLines { most: limits.most_lines }));
        }
        let version = Line { table: &mut table, scratch: &mut scratch, no, len: line.len(), alloc }.read(name, line)?;
        parsed.push((version, (table.versions.len() - 1) as u32));
    }
    let mut order = Vec::with_capacity_in(parsed.len(), alloc);
    {
        let table = &table;
        let vers = |row: u32| table.str(table.versions[row as usize].vers);
        parsed.as_mut_slice().sort_unstable_by(|a, b| {
            a.0.cmp_precedence(&b.0).then_with(|| vers(a.1).cmp(vers(b.1))).then(a.1.cmp(&b.1))
        });
        for (_, row) in parsed.as_slice() {
            order.push(*row);
        }
    }
    drop(parsed);
    table.into_record_in(order.as_slice(), name, validator, alloc).map_err(|too| {
        refused(0, Field::File, Bad::TooLarge { len: too.len, most: u64::from(u32::MAX) })
    })
}

/// One line being read into the table.
struct Line<'t, 's, 'a> {
    table: &'t mut Table<'a>,
    /// A key's decoded text.
    scratch: &'s mut String<Alloc<'a>>,
    no: u32,
    len: usize,
    alloc: Alloc<'a>,
}

impl<'a> Line<'_, '_, 'a> {
    fn refused(&self, field: Field, bad: Bad) -> IndexRefused {
        refused(self.no, field, bad)
    }

    fn next<'j>(&self, reader: &mut Reader<'j, Alloc<'a>>) -> Result<Event<'j>, IndexRefused> {
        match reader.next() {
            Ok(Some(item)) => Ok(item.event),
            // The reader ends only after the top value is whole; within it, more is wanted.
            Ok(None) => Err(json(self.no, blaze_json::Error { at: self.len, what: What::End { expected: Expected::Value } })),
            Err(error) => Err(json(self.no, error)),
        }
    }

    /// The rest of the value `first` began.
    fn skip<'j>(&self, reader: &mut Reader<'j, Alloc<'a>>, first: Event<'j>) -> Result<(), IndexRefused> {
        if !matches!(first, Event::StartMap | Event::StartList) {
            return Ok(());
        }
        let base = reader.depth() - 1;
        while reader.depth() > base {
            self.next(reader)?;
        }
        Ok(())
    }

    /// A key's text, decoded into the scratch buffer.
    fn word(&mut self, key: &Str<'_>) -> &str {
        self.scratch.clear();
        key.decode_into(self.scratch);
        self.scratch.as_str()
    }

    /// The line read into one row; its version, parsed, for the file's order.
    fn read(mut self, name: &str, line: &[u8]) -> Result<blaze_semver::Version<Alloc<'a>>, IndexRefused> {
        let mut reader = Reader::new_in(line, LINE_LIMITS, self.alloc).map_err(|error| json(self.no, error))?;
        if !matches!(self.next(&mut reader)?, Event::StartMap) {
            return Err(self.refused(Field::Json, Bad::NotAnObject));
        }
        let marks = (self.table.deps.len(), self.table.features.len(), self.table.values.len());
        let mut row = VRow {
            name: Span::NONE,
            vers: Span::NONE,
            cksum: [0; 32],
            links: Span::NONE,
            rust_version: Span::NONE,
            pubtime: Span::NONE,
            deps: Run::EMPTY,
            features: Run::EMPTY,
            line: self.no,
            v: 1,
            status: Status::Candidate,
        };
        let mut seen = 0u32;
        let mut invalid: Option<Field> = None;
        let mut yanked = false;
        loop {
            let Event::Key(key) = self.next(&mut reader)? else {
                // In an object the reader gives a key or its end, nothing else.
                break;
            };
            let Some(field) = Field::top(self.word(&key)) else {
                let value = self.next(&mut reader)?;
                self.skip(&mut reader, value)?;
                continue;
            };
            if seen & field.bit() != 0 {
                return Err(self.refused(field, Bad::Twice));
            }
            seen |= field.bit();
            let value = self.next(&mut reader)?;
            match (field, value) {
                (Field::Name, Event::Text(text)) => row.name = self.table.text(&text),
                (Field::Vers, Event::Text(text)) => row.vers = self.table.text(&text),
                (Field::Name | Field::Vers, _) => return Err(self.refused(field, Bad::WrongType)),
                (Field::Deps, Event::StartList) => self.deps(&mut reader, &mut invalid)?,
                (Field::Cksum, Event::Text(text)) => match hex32(&text) {
                    Some(sum) => row.cksum = sum,
                    None => {
                        invalid.get_or_insert(Field::Cksum);
                    }
                },
                (Field::Features | Field::Features2, Event::StartMap) => self.features(&mut reader, field, &mut invalid)?,
                (Field::Yanked, Event::Bool(it)) => yanked = it,
                (Field::Links, Event::Text(text)) => row.links = self.table.text(&text),
                (Field::RustVersion, Event::Text(text)) => row.rust_version = self.table.text(&text),
                (Field::Pubtime, Event::Text(text)) => {
                    let span = self.table.text(&text);
                    match is_pubtime(self.table.str(span)) {
                        true => row.pubtime = span,
                        false => {
                            invalid.get_or_insert(Field::Pubtime);
                        }
                    }
                }
                (Field::V, Event::Int(n)) => match n.as_u64().and_then(|n| u32::try_from(n).ok()) {
                    Some(v) => row.v = v,
                    None => {
                        invalid.get_or_insert(Field::V);
                    }
                },
                // cargo's `Option`s: null is none.
                (Field::Features2 | Field::Yanked | Field::Links | Field::RustVersion | Field::Pubtime | Field::V, Event::Null) => {}
                (field, other) => {
                    invalid.get_or_insert(field);
                    self.skip(&mut reader, other)?;
                }
            }
        }
        match reader.next() {
            Ok(None) => {}
            Ok(Some(_)) => return Err(self.refused(Field::Json, Bad::NotAnObject)),
            Err(error) => return Err(json(self.no, error)),
        }
        if row.name.is_none() {
            return Err(self.refused(Field::Name, Bad::Missing));
        }
        if row.vers.is_none() {
            return Err(self.refused(Field::Vers, Bad::Missing));
        }
        if !self.table.str(row.name).eq_ignore_ascii_case(name) {
            return Err(self.refused(Field::Name, Bad::OtherCrate));
        }
        let version = blaze_semver::Version::parse_in(self.table.str(row.vers), blaze_semver::Limits::CARGO, self.alloc)
            .map_err(|_| self.refused(Field::Vers, Bad::NotAVersion))?;
        if seen & Field::Deps.bit() == 0 {
            invalid.get_or_insert(Field::Deps);
        }
        if seen & Field::Cksum.bit() == 0 {
            invalid.get_or_insert(Field::Cksum);
        }
        let (deps, features, values) = marks;
        match invalid {
            // As cargo's `Invalid`: the name, version and `rust_version` alone.
            Some(_) => {
                self.table.deps.truncate(deps);
                self.table.features.truncate(features);
                self.table.values.truncate(values);
            }
            None => {
                row.deps = Run { first: deps as u32, count: (self.table.deps.len() - deps) as u32 };
                row.features = Run { first: features as u32, count: (self.table.features.len() - features) as u32 };
            }
        }
        row.status = match invalid {
            _ if row.v > INDEX_V_MAX => Status::Unsupported(row.v),
            Some(field) => Status::Invalid(field),
            None if yanked => Status::Yanked,
            None => Status::Candidate,
        };
        self.table.versions.push(row);
        Ok(version)
    }

    /// `deps`, after its `[`.
    fn deps<'j>(&mut self, reader: &mut Reader<'j, Alloc<'a>>, invalid: &mut Option<Field>) -> Result<(), IndexRefused> {
        loop {
            match self.next(reader)? {
                Event::EndList => return Ok(()),
                Event::StartMap => self.dep(reader, invalid)?,
                other => {
                    invalid.get_or_insert(Field::Dep);
                    self.skip(reader, other)?;
                }
            }
        }
    }

    /// One dependency, after its `{`; kept only when it is whole.
    fn dep<'j>(&mut self, reader: &mut Reader<'j, Alloc<'a>>, invalid: &mut Option<Field>) -> Result<(), IndexRefused> {
        let mut dep = DRow {
            name: Span::NONE,
            package: Span::NONE,
            req: Span::NONE,
            target: Span::NONE,
            registry: Span::NONE,
            features: Run::EMPTY,
            kind: DepKind::Normal,
            flags: FLAG_DEFAULT_FEATURES,
        };
        let mut seen = 0u32;
        let mut whole = true;
        loop {
            let Event::Key(key) = self.next(reader)? else { break };
            let Some(field) = Field::dep(self.word(&key)) else {
                let value = self.next(reader)?;
                self.skip(reader, value)?;
                continue;
            };
            if seen & field.bit() != 0 {
                return Err(self.refused(field, Bad::Twice));
            }
            seen |= field.bit();
            let value = self.next(reader)?;
            let set = |flags: &mut u8, flag: u8, on: bool| match on {
                true => *flags |= flag,
                false => *flags &= !flag,
            };
            match (field, value) {
                (Field::DepName, Event::Text(text)) => dep.name = self.table.text(&text),
                (Field::DepReq, Event::Text(text)) => {
                    dep.req = self.table.text(&text);
                    let req = self.table.str(dep.req);
                    if blaze_semver::VersionReq::parse_in(req, blaze_semver::Limits::CARGO, self.alloc).is_err() {
                        invalid.get_or_insert(Field::DepReq);
                        whole = false;
                    }
                }
                (Field::DepFeatures, Event::StartList) => {
                    let first = self.table.values.len();
                    loop {
                        match self.next(reader)? {
                            Event::EndList => break,
                            // Left out, as cargo leaves out the empty features older
                            // registries wrote.
                            Event::Text(text) if text.raw().is_empty() => {}
                            Event::Text(text) => {
                                let span = self.table.text(&text);
                                self.table.values.push(span);
                            }
                            other => {
                                invalid.get_or_insert(Field::DepFeatures);
                                whole = false;
                                self.skip(reader, other)?;
                            }
                        }
                    }
                    dep.features = Run { first: first as u32, count: (self.table.values.len() - first) as u32 };
                }
                (Field::DepOptional, Event::Bool(on)) => set(&mut dep.flags, FLAG_OPTIONAL, on),
                (Field::DepDefaultFeatures, Event::Bool(on)) => set(&mut dep.flags, FLAG_DEFAULT_FEATURES, on),
                (Field::DepPublic, Event::Bool(on)) => set(&mut dep.flags, FLAG_PUBLIC, on),
                (Field::DepTarget, Event::Text(text)) => dep.target = self.table.text(&text),
                (Field::DepRegistry, Event::Text(text)) => dep.registry = self.table.text(&text),
                (Field::DepPackage, Event::Text(text)) => dep.package = self.table.text(&text),
                (Field::DepKind, Event::Text(text)) => {
                    dep.kind = match self.word(&text) {
                        "dev" => DepKind::Development,
                        "build" => DepKind::Build,
                        _ => DepKind::Normal,
                    }
                }
                (Field::DepArtifact, Event::StartList) => {
                    dep.flags |= FLAG_ARTIFACT;
                    self.skip(reader, Event::StartList)?;
                }
                (Field::DepTarget | Field::DepRegistry | Field::DepPackage | Field::DepKind | Field::DepPublic | Field::DepArtifact, Event::Null) => {}
                (field, other) => {
                    invalid.get_or_insert(field);
                    whole = false;
                    self.skip(reader, other)?;
                }
            }
        }
        if dep.name.is_none() {
            invalid.get_or_insert(Field::DepName);
            whole = false;
        }
        if dep.req.is_none() {
            invalid.get_or_insert(Field::DepReq);
            whole = false;
        }
        if whole {
            self.table.deps.push(dep);
        }
        Ok(())
    }

    /// `features` or `features2`, after its `{`.
    fn features<'j>(&mut self, reader: &mut Reader<'j, Alloc<'a>>, field: Field, invalid: &mut Option<Field>) -> Result<(), IndexRefused> {
        loop {
            let Event::Key(key) = self.next(reader)? else { return Ok(()) };
            let name = self.table.text(&key);
            match self.next(reader)? {
                Event::StartList => {
                    let first = self.table.values.len();
                    loop {
                        match self.next(reader)? {
                            Event::EndList => break,
                            Event::Text(text) => {
                                let span = self.table.text(&text);
                                self.table.values.push(span);
                            }
                            other => {
                                invalid.get_or_insert(field);
                                self.skip(reader, other)?;
                            }
                        }
                    }
                    let values = Run { first: first as u32, count: (self.table.values.len() - first) as u32 };
                    self.table.features.push(FRow { name, values });
                }
                other => {
                    invalid.get_or_insert(field);
                    self.skip(reader, other)?;
                }
            }
        }
    }
}

/// 64 hex digits as 32 bytes.
fn hex32(text: &Str<'_>) -> Option<[u8; 32]> {
    let digits = text.as_plain()?.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let nibble = |d: u8| match d {
        b'0'..=b'9' => Some(d - b'0'),
        b'a'..=b'f' => Some(d - b'a' + 10),
        b'A'..=b'F' => Some(d - b'A' + 10),
        _ => None,
    };
    let mut out = [0; 32];
    for (at, pair) in digits.chunks_exact(2).enumerate() {
        out[at] = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

/// Whether `text` is a time as cargo's `parse_pubtime` takes one: `%Y-%m-%dT%H:%M:%SZ`, padded,
/// of a day that exists (blaze-units' `is_pubtime`, kept).
pub(crate) fn is_pubtime(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 20 {
        return false;
    }
    let shape = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];
    if shape.iter().any(|&(at, byte)| bytes[at] != byte) {
        return false;
    }
    let digits = |from: usize, to: usize| -> Option<u32> {
        let mut n = 0u32;
        for &d in &bytes[from..to] {
            if !d.is_ascii_digit() {
                return None;
            }
            n = n * 10 + u32::from(d - b'0');
        }
        Some(n)
    };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) =
        (digits(0, 4), digits(5, 7), digits(8, 10), digits(11, 13), digits(14, 16), digits(17, 19))
    else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}
