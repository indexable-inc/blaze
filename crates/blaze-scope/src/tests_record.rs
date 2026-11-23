//! A test unit's tests as a record of their own (docs/v2/plan/design/test.md, "The tests
//! record"): one row per `#[test]` (and `#[bench]`) function of ONE unit compiled with `test`
//! on: its item key, its path as printed (`module::name`, the crate's name left off), whether it
//! is ignored and why, whether it should panic and the text its panic message must hold.
//!
//! Built from ATTRIBUTES ALONE, when the unit's names are solved: no body is lowered, typed or
//! built to list a unit's tests. The runner's entry is generated over this record, and an edit
//! that changes no attribute and no test's key leaves these bytes equal.
//!
//! The rules read, from the Reference, "Testing attributes":
//! - "The `test` attribute marks a function to be executed as a test. These functions are only
//!   compiled when in test mode."
//! - "A function annotated with the `test` attribute can also be annotated with the `ignore`
//!   attribute. The `ignore` attribute tells the test harness to not execute that function as
//!   a test." "The `ignore` attribute may optionally be written with the MetaNameValueStr
//!   syntax to specify a reason why the test is ignored."
//! - "A function annotated with the `test` attribute that returns `()` can also be annotated
//!   with the `should_panic` attribute. The `should_panic` attribute makes the test only pass
//!   if it actually panics." "The `should_panic` attribute may optionally take an input string
//!   that must appear within the panic message": written `#[should_panic = "text"]` or
//!   `#[should_panic(expected = "text")]`.
//! - "Conditional compilation", `cfg_attr`: a true predicate's attributes stand in its place,
//!   so `#[cfg_attr(miri, ignore)]` is `ignore` exactly when the unit's cfg holds `miri`.
//!
//! `test` is read as the built-in whether written bare or by its prelude path
//! (`::core::prelude::v1::test`, what an attribute macro such as `tokio::test` writes on the
//! function it returns): that function is an item of the call's expansion, and listed here
//! under its own key. A path this does not know (`tokio::test` itself) is not `test`.
//!
//! The form (integers little-endian; the record 16-aligned): a 64-byte header (`BZTS`, version,
//! the row count, the strings' offset and length, zeros to 32, the unit's crate key at 32);
//! the rows, FIXED 64 bytes, sorted by (path bytes, key): the item key (32), then u32 fields:
//! the path's offset and length in the strings, the expected text's, the ignore reason's, the
//! flags, zero; then the strings (UTF-8, in the order of the items' keys, so any worker in any
//! order writes equal bytes). A reader checks the header and bounds and finds a test by its
//! path with a binary search in place.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{AttrKind, ItemDetail, RecordBytes};

use crate::resolve::CrateNames;
use crate::task::{SplitTop, args, head, prelude_attr, strip_attr};

const MAGIC: [u8; 4] = *b"BZTS";
const VERSION: u32 = 1;
const ALIGN: usize = 16;
const HEADER: usize = 64;
const KEY_AT: usize = 32;
const ROW: usize = 64;

/// `#[ignore]`.
const IGNORE: u32 = 1;
/// `#[ignore = "reason"]`: the reason is in the strings.
const IGNORE_REASON: u32 = 1 << 1;
/// `#[should_panic]`.
const SHOULD_PANIC: u32 = 1 << 2;
/// `#[should_panic = ".."]` or `#[should_panic(expected = "..")]`: the text is in the strings.
const EXPECTED: u32 = 1 << 3;
/// `#[bench]`, not `#[test]`.
const BENCH: u32 = 1 << 4;
const KNOWN_FLAGS: u32 = IGNORE | IGNORE_REASON | SHOULD_PANIC | EXPECTED | BENCH;

/// Why a tests record was not written or not read. Each names the unit or the function and the
/// rule that refused it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TestsRecordError {
    /// The unit's cfg does not hold `test`: its `#[test]` functions are no part of it (the
    /// Reference: "only compiled when in test mode"), so it has no tests record.
    NotATestUnit { krate: CrateItemKey },
    /// A section's size does not fit its u32 field.
    TooBig { what: &'static str },
    /// An item's text has no outline entry: the names are not this crate's.
    NoOutline { item: ItemKey },
    /// A test function's module has no path, or the function no name.
    NoPath { item: ItemKey },
    /// A function's `cfg_attr` predicate did not read: whether it is a test, ignored or should
    /// panic is not known, and is never guessed.
    CfgUnread { item: ItemKey },
    /// `#[test]` and `#[bench]` on one function: it is a test or a bench, and which is not
    /// guessed. (One of them twice is one test: [`TestsSection::duplicated`].)
    TestAndBench { item: ItemKey },
    /// `#[test]` on a function that is no free function (a member of an impl or trait): the
    /// Reference's attribute is for "free functions that are monomorphic".
    NotAFreeFn { item: ItemKey },
    /// An `ignore` or `should_panic` this does not read: neither bare, `= "text"` nor (for
    /// `should_panic`) `(expected = "text")` with one string literal.
    Form { item: ItemKey, attr: &'static str },
    /// The bytes do not start on a 16-byte boundary: map or copy them to one.
    Unaligned,
    /// Not `BZTS` and this version.
    NotThisRecord,
    /// A section runs past the end of the record.
    Short { what: &'static str, at: usize },
    /// A row's string is outside the strings, is not UTF-8, or its flags hold an unknown bit.
    Row { row: usize, what: &'static str },
}

impl core::fmt::Display for TestsRecordError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TestsRecordError::NotATestUnit { krate } => write!(f, "the unit {krate:?} is not compiled with `test`: it has no tests record"),
            TestsRecordError::TooBig { what } => write!(f, "tests record: {what} does not fit a u32"),
            TestsRecordError::NoOutline { item } => write!(f, "tests record: the item {item:?} has no outline entry"),
            TestsRecordError::NoPath { item } => write!(f, "tests record: the test function {item:?} has no module path or no name"),
            TestsRecordError::CfgUnread { item } => write!(f, "tests record: a `cfg_attr` predicate on the function {item:?} did not read"),
            TestsRecordError::TestAndBench { item } => write!(f, "tests record: the function {item:?} is marked both `test` and `bench`"),
            TestsRecordError::NotAFreeFn { item } => write!(f, "tests record: `#[test]` on {item:?}, which is no free function"),
            TestsRecordError::Form { item, attr } => write!(f, "tests record: the `{attr}` attribute of {item:?} is in a form not read"),
            TestsRecordError::Unaligned => write!(f, "tests record: the bytes are not 16-aligned"),
            TestsRecordError::NotThisRecord => write!(f, "tests record: not `BZTS` version {VERSION}"),
            TestsRecordError::Short { what, at } => write!(f, "tests record: {what} at {at} run past the end"),
            TestsRecordError::Row { row, what } => write!(f, "tests record: row {row}: {what}"),
        }
    }
}

impl TestsRecordError {
    /// The function a refusal is about, when it is about one function and not the unit or the
    /// record: such a refusal leaves that function out of the record and keeps the rest
    /// ([`TestsSection::refused`]), so one function's attributes never empty a unit's tests.
    pub fn item(&self) -> Option<ItemKey> {
        match self {
            TestsRecordError::NoPath { item }
            | TestsRecordError::CfgUnread { item }
            | TestsRecordError::TestAndBench { item }
            | TestsRecordError::NotAFreeFn { item }
            | TestsRecordError::Form { item, .. } => Some(*item),
            _ => None,
        }
    }
}

/// A test unit's tests record, and what it says about single functions beside it.
pub struct TestsSection<'b> {
    /// The record ([`TestsRecord::open`] reads it).
    pub bytes: RecordBytes<'b>,
    /// Each function refused as a test (an error at that function; [`TestsRecordError::item`]
    /// names it), sorted by key: it has no row, and every other test keeps its own.
    pub refused: AVec<'b, TestsRecordError>,
    /// Each test whose attributes hold a bare `#[test]` (or `#[bench]`) after another marker
    /// of its kind, sorted by key: one row, and rustc's warning `duplicate_macro_attributes`
    /// ("duplicated attribute") at the function.
    pub duplicated: AVec<'b, ItemKey>,
}

fn u32_of(n: usize, what: &'static str) -> Result<u32, TestsRecordError> {
    u32::try_from(n).map_err(|_| TestsRecordError::TooBig { what })
}

fn put32(out: &mut RecordBytes<'_>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

/// One string literal at the start of `s` (after blanks), its value pushed onto `out`: a plain
/// literal with the Reference's escapes ("Tokens", "String literals": quote, ASCII, Unicode and
/// the line continuation) or a raw one. Returns what follows it; `None` for any other form.
fn literal<'t>(s: &'t str, out: &mut RecordBytes<'_>) -> Option<&'t str> {
    let s = s.trim_start();
    let b = s.as_bytes();
    if b.first() == Some(&b'r') {
        let hashes = b[1..].iter().take_while(|c| **c == b'#').count();
        if b.get(1 + hashes) != Some(&b'"') {
            return None;
        }
        let body = 2 + hashes;
        let mut at = body;
        while at < b.len() {
            if b[at] == b'"' && b[at + 1..].len() >= hashes && b[at + 1..at + 1 + hashes].iter().all(|c| *c == b'#') {
                out.extend_from_slice(&b[body..at]);
                return Some(&s[at + 1 + hashes..]);
            }
            at += 1;
        }
        return None;
    }
    if b.first() != Some(&b'"') {
        return None;
    }
    let mut at = 1;
    while at < b.len() {
        match b[at] {
            b'"' => return Some(&s[at + 1..]),
            b'\\' => {
                let c = *b.get(at + 1)?;
                at += 2;
                match c {
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'\\' => out.push(b'\\'),
                    b'0' => out.push(0),
                    b'\'' => out.push(b'\''),
                    b'"' => out.push(b'"'),
                    b'x' => {
                        let hex = core::str::from_utf8(b.get(at..at + 2)?).ok()?;
                        let value = u8::from_str_radix(hex, 16).ok()?;
                        if value > 0x7f {
                            return None;
                        }
                        out.push(value);
                        at += 2;
                    }
                    b'u' => {
                        if b.get(at) != Some(&b'{') {
                            return None;
                        }
                        at += 1;
                        let (mut value, mut digits) = (0u32, 0usize);
                        loop {
                            let d = *b.get(at)?;
                            at += 1;
                            match d {
                                b'}' => break,
                                b'_' => {}
                                _ => {
                                    value = value.checked_mul(16)?.checked_add((d as char).to_digit(16)?)?;
                                    digits += 1;
                                }
                            }
                        }
                        if digits == 0 || digits > 6 {
                            return None;
                        }
                        let mut utf8 = [0u8; 4];
                        out.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut utf8).as_bytes());
                    }
                    b'\n' => {
                        while at < b.len() && matches!(b[at], b' ' | b'\t' | b'\n' | b'\r') {
                            at += 1;
                        }
                    }
                    _ => return None,
                }
            }
            c => {
                out.push(c);
                at += 1;
            }
        }
    }
    None
}

/// What follows `= ` in `rest` when it is one string literal and nothing more: the literal's
/// value is pushed onto `out`.
fn name_value(rest: &str, out: &mut RecordBytes<'_>) -> Option<()> {
    let after = literal(rest.trim_start().strip_prefix('=')?, out)?;
    after.trim().is_empty().then_some(())
}

/// A test function while the record is built.
#[derive(Clone, Copy)]
struct Found {
    key: ItemKey,
    path: (u32, u32),
    expected: (u32, u32),
    reason: (u32, u32),
    flags: u32,
}

/// The tests record of `names`, a unit solved with `test` in its cfg. A unit without it is
/// refused ([`TestsRecordError::NotATestUnit`]); a test unit with no test writes zero rows.
///
/// A function's own refusal ([`TestsRecordError::item`]) leaves that function out and is
/// returned beside the record ([`TestsSection::refused`]); only a refusal of the unit or the
/// record is an `Err`.
///
/// Markers on one function: the Reference, "Testing attributes", says "The `test` attribute
/// marks a function to be executed as a test", and rustc expands each `test` attribute on its
/// own, so a function carrying two (tempo's e2e suite: `#[tokio::test]` writes
/// `#[::core::prelude::v1::test]` and commonware's `#[test_traced]` writes `#[test]`) is
/// registered twice by rustc, with no change to its environment (the lint listing's words
/// for `duplicate_macro_attributes`). Here it is ONE row: a second run of the same function
/// tells nothing the first did not, and a path names one test. rustc warns
/// (`duplicate_macro_attributes`, warn by default) when the attribute it expands is followed by
/// one whose path is the bare name (rustc_builtin_macros `warn_on_duplicate_attribute`, by
/// `attr::find_by_name`): so a bare `test` after an earlier test marker is
/// [`TestsSection::duplicated`], and `#[test] #[::core::prelude::v1::test]` is silent.
pub fn encode<'b>(names: &CrateNames<'_>, alloc: Alloc<'b>) -> Result<TestsSection<'b>, TestsRecordError> {
    if names.cfg.eval("test") != Some(true) {
        return Err(TestsRecordError::NotATestUnit { krate: names.key });
    }
    let tables = &names.tables;
    // By key: the strings' order is the items' key order, whatever the map's.
    let mut items: AVec<'b, ItemKey> = AVec::new_in(alloc);
    items.extend(tables.texts.keys().copied());
    items.sort_unstable();

    let mut strings = RecordBytes::new_in(alloc);
    let mut found: AVec<'b, Found> = AVec::new_in(alloc);
    let mut refused: AVec<'b, TestsRecordError> = AVec::new_in(alloc);
    let mut duplicated: AVec<'b, ItemKey> = AVec::new_in(alloc);
    for key in items.iter() {
        let mark = strings.len();
        match test_of(names, *key, &mut strings, alloc) {
            Ok(Some((test, twice))) => {
                found.push(test);
                if twice {
                    duplicated.push(*key);
                }
            }
            Ok(None) => strings.truncate(mark),
            Err(error) if error.item().is_some() => {
                strings.truncate(mark);
                refused.push(error);
            }
            Err(error) => return Err(error),
        }
    }

    let of = |s: (u32, u32)| &strings[s.0 as usize..(s.0 + s.1) as usize];
    found.sort_unstable_by(|a, b| of(a.path).cmp(of(b.path)).then(a.key.cmp(&b.key)));

    let mut out = RecordBytes::new_in(alloc);
    out.extend_from_slice(&MAGIC);
    put32(&mut out, VERSION);
    put32(&mut out, u32_of(found.len(), "a unit's tests")?);
    put32(&mut out, u32_of(HEADER + found.len() * ROW, "the rows")?);
    put32(&mut out, u32_of(strings.len(), "the strings")?);
    out.resize(KEY_AT, 0);
    out.extend_from_slice(&names.key.0.0);
    for t in found.iter() {
        out.extend_from_slice(&t.key.0.0);
        for n in [t.path.0, t.path.1, t.expected.0, t.expected.1, t.reason.0, t.reason.1, t.flags, 0] {
            put32(&mut out, n);
        }
    }
    out.extend_from_slice(&strings);
    out.resize(out.len().next_multiple_of(ALIGN), 0);
    Ok(TestsSection { bytes: out, refused, duplicated })
}

/// The test the item `key` is, its strings pushed onto `strings` (the caller takes them back
/// out unless a test is returned), and whether rustc warns its markers duplicated; `None` for
/// an item that is no test.
fn test_of<'b>(names: &CrateNames<'_>, key: ItemKey, strings: &mut RecordBytes<'b>, alloc: Alloc<'b>) -> Result<Option<(Found, bool)>, TestsRecordError> {
    let key = &key;
    let (it, at) = &names.tables.texts[key];
    let (text, outline) = names.outline_at(*at).ok_or(TestsRecordError::NoOutline { item: *key })?;
    let entry = outline.items().get(it.item.0 as usize).ok_or(TestsRecordError::NoOutline { item: *key })?;
    if !matches!(entry.detail, ItemDetail::Fn { .. }) {
        return Ok(None);
    }
    {
        // What the function's attributes say, a true `cfg_attr`'s attributes in its place, in
        // order (a work list: the stack holds the attributes still to read, next on top).
        let (mut test, mut bench) = (0u32, 0u32);
        let mut twice = false;
        let mut flags = 0u32;
        let (mut expected, mut reason) = ((0u32, 0u32), (0u32, 0u32));
        let mut stack: AVec<'b, &str> = AVec::new_in(alloc);
        for attr in outline.attrs(entry).iter() {
            if attr.kind == AttrKind::Doc {
                continue;
            }
            stack.push(strip_attr(entry.text(text, attr.range)));
            while let Some(content) = stack.pop() {
                let path = head(content);
                let rest = content[path.len()..].trim();
                match prelude_attr(path) {
                    "cfg_attr" => {
                        let mut parts = SplitTop::new(args(content));
                        let Some(predicate) = parts.next() else { continue };
                        match names.cfg.eval(predicate) {
                            None => return Err(TestsRecordError::CfgUnread { item: *key }),
                            Some(false) => {}
                            Some(true) => {
                                let start = stack.len();
                                stack.extend(parts.map(str::trim).filter(|p| !p.is_empty()));
                                stack[start..].reverse();
                            }
                        }
                    }
                    "unsafe" => stack.push(args(content).trim()),
                    // A bare name after a marker of its kind: rustc's duplicated attribute.
                    "test" => {
                        twice |= test > 0 && path.trim() == "test";
                        test += 1;
                    }
                    "bench" => {
                        twice |= bench > 0 && path.trim() == "bench";
                        bench += 1;
                    }
                    "ignore" => {
                        flags |= IGNORE;
                        if !rest.is_empty() {
                            let start = strings.len();
                            name_value(rest, strings).ok_or(TestsRecordError::Form { item: *key, attr: "ignore" })?;
                            reason = (u32_of(start, "the strings")?, u32_of(strings.len() - start, "an ignore reason")?);
                            flags |= IGNORE_REASON;
                        }
                    }
                    "should_panic" => {
                        flags |= SHOULD_PANIC;
                        if !rest.is_empty() {
                            let refused = || TestsRecordError::Form { item: *key, attr: "should_panic" };
                            let start = strings.len();
                            if rest.starts_with('=') {
                                name_value(rest, strings).ok_or_else(refused)?;
                            } else {
                                // `(expected = "text")`, a trailing comma allowed.
                                let inside = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')).ok_or_else(refused)?;
                                let value = inside.trim_start().strip_prefix("expected").ok_or_else(refused)?;
                                let after = value.trim_start().strip_prefix('=').and_then(|v| literal(v, strings)).ok_or_else(refused)?;
                                if !matches!(after.trim(), "" | ",") {
                                    return Err(refused());
                                }
                            }
                            expected = (u32_of(start, "the strings")?, u32_of(strings.len() - start, "an expected text")?);
                            flags |= EXPECTED;
                        }
                    }
                    _ => {}
                }
            }
        }
        if test + bench == 0 {
            // Its `ignore` or `should_panic` strings, if any, are no test's: the caller takes
            // them back out.
            return Ok(None);
        }
        if test > 0 && bench > 0 {
            return Err(TestsRecordError::TestAndBench { item: *key });
        }
        if let Some(parent) = entry.parent {
            if !matches!(outline.item(parent).detail, ItemDetail::Mod { .. }) {
                return Err(TestsRecordError::NotAFreeFn { item: *key });
            }
        }
        if bench > 0 {
            flags |= BENCH;
        }
        // `module::name`: the module's path without the crate's name before it.
        let module = names.path_of(it.module).ok_or(TestsRecordError::NoPath { item: *key })?;
        let name = it.name.ok_or(TestsRecordError::NoPath { item: *key })?;
        let start = strings.len();
        if let Some((_, under)) = module.split_once("::") {
            strings.extend_from_slice(under.as_bytes());
            strings.extend_from_slice(b"::");
        }
        strings.extend_from_slice(name.as_bytes());
        let path = (u32_of(start, "the strings")?, u32_of(strings.len() - start, "a test's path")?);
        Ok(Some((Found { key: *key, path, expected, reason, flags }, twice)))
    }
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(b)
}

fn key_at(bytes: &[u8], at: usize) -> ItemKey {
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes[at..at + 32]);
    ItemKey(ContentId(k))
}

/// What a test must do to pass, beside returning.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShouldPanic<'a> {
    /// It must not panic.
    No,
    /// It must panic.
    Yes,
    /// It must panic with a message that holds this text.
    WithMessage(&'a str),
}

/// One test of a unit, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TestRow<'a> {
    /// The function's item key: what its instance is built from.
    pub key: ItemKey,
    /// As printed: `module::name`.
    pub path: &'a str,
    /// `Some` when `#[ignore]`: the reason, empty when none is written.
    pub ignore: Option<&'a str>,
    pub should_panic: ShouldPanic<'a>,
    /// `#[bench]`, not `#[test]`.
    pub bench: bool,
}

/// A tests record, open: the header and bounds checked, nothing parsed.
#[derive(Clone, Copy, Debug)]
pub struct TestsRecord<'a> {
    bytes: &'a [u8],
    count: usize,
    strings_at: usize,
    strings_len: usize,
}

impl<'a> TestsRecord<'a> {
    /// Checks the header and that the rows and the strings lie inside `bytes`.
    pub fn open(bytes: &'a [u8]) -> Result<TestsRecord<'a>, TestsRecordError> {
        if bytes.as_ptr() as usize % ALIGN != 0 {
            return Err(TestsRecordError::Unaligned);
        }
        if bytes.len() < HEADER || bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
            return Err(TestsRecordError::NotThisRecord);
        }
        let (count, strings_at, strings_len) = (le32(bytes, 8) as usize, le32(bytes, 12) as usize, le32(bytes, 16) as usize);
        let rows_end = count.checked_mul(ROW).and_then(|n| n.checked_add(HEADER));
        if rows_end != Some(strings_at) {
            return Err(TestsRecordError::Short { what: "the test rows", at: HEADER });
        }
        if !strings_at.checked_add(strings_len).is_some_and(|end| end <= bytes.len()) {
            return Err(TestsRecordError::Short { what: "the strings", at: strings_at });
        }
        Ok(TestsRecord { bytes, count, strings_at, strings_len })
    }

    /// The unit's crate key.
    pub fn key(&self) -> CrateItemKey {
        CrateItemKey(key_at(self.bytes, KEY_AT).0)
    }

    /// How many tests (and benches) the unit has.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn string(&self, row: usize, at: usize, what: &'static str) -> Result<&'a str, TestsRecordError> {
        let bytes: &'a [u8] = self.bytes;
        let (start, len) = (le32(bytes, at) as usize, le32(bytes, at + 4) as usize);
        if !start.checked_add(len).is_some_and(|end| end <= self.strings_len) {
            return Err(TestsRecordError::Row { row, what });
        }
        core::str::from_utf8(&bytes[self.strings_at + start..self.strings_at + start + len]).map_err(|_| TestsRecordError::Row { row, what })
    }

    /// The `row`th test in path order; `None` past the last.
    pub fn row(&self, row: usize) -> Result<Option<TestRow<'a>>, TestsRecordError> {
        if row >= self.count {
            return Ok(None);
        }
        let at = HEADER + row * ROW;
        let flags = le32(self.bytes, at + 56);
        if flags & !KNOWN_FLAGS != 0 {
            return Err(TestsRecordError::Row { row, what: "its flags hold a bit this version does not know" });
        }
        let ignore = match (flags & IGNORE != 0, flags & IGNORE_REASON != 0) {
            (false, false) => None,
            (true, false) => Some(""),
            (true, true) => Some(self.string(row, at + 48, "its ignore reason is outside the strings or not UTF-8")?),
            (false, true) => return Err(TestsRecordError::Row { row, what: "an ignore reason on a test that is not ignored" }),
        };
        let should_panic = match (flags & SHOULD_PANIC != 0, flags & EXPECTED != 0) {
            (false, false) => ShouldPanic::No,
            (true, false) => ShouldPanic::Yes,
            (true, true) => ShouldPanic::WithMessage(self.string(row, at + 40, "its expected text is outside the strings or not UTF-8")?),
            (false, true) => return Err(TestsRecordError::Row { row, what: "an expected text on a test that should not panic" }),
        };
        Ok(Some(TestRow {
            key: key_at(self.bytes, at),
            path: self.string(row, at + 32, "its path is outside the strings or not UTF-8")?,
            ignore,
            should_panic,
            bench: flags & BENCH != 0,
        }))
    }

    /// The first test printed as `path`, found by a binary search of the rows in place.
    pub fn find(&self, path: &str) -> Result<Option<TestRow<'a>>, TestsRecordError> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            let at = HEADER + mid * ROW;
            if self.string(mid, at + 32, "its path is outside the strings or not UTF-8")?.as_bytes() < path.as_bytes() {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        match self.row(low)? {
            Some(found) if found.path == path => Ok(Some(found)),
            _ => Ok(None),
        }
    }
}
