//! `pins/base-tools.json`, form 1: the pinned base tool set of each platform (D192), read with
//! the tree's no_std pull reader ([`blaze_json::Reader`]) and written with its event writer.
//!
//! The schema (every key once, each required, nothing else; an unknown key is refused):
//!
//! ```text
//! {
//!   "comment": <string>,
//!   "form": 1,
//!   "release": "https://..." | "PENDING",          where the archives are fetched from
//!   "platforms": {
//!     "<triple>": {                                  one of [`Platform::ALL`]
//!       "archive": "<file name>",                    [A-Za-z0-9._+-], no folder
//!       "digest": "<64 lower-case hex>" | "PENDING", the archive's SHA-256
//!       "size": <bytes> | "PENDING",                 pending exactly when `digest` is
//!       "programs": { "<group>": ["<name>", ...] },  the archive's bin/, each name once
//!       "answers": {
//!         "hostname": "<text>",
//!         "uname": { "m": .., "o": .., "p": .., "r": .., "s": .., "v": .. }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! A group's name is `[a-z_]` words and says nothing to the reader: the set is the names of
//! every group, read sorted ([`Row::programs`]), so two files listing one set in different
//! orders give equal sets. An answer is printable ASCII (it is a line of `etc/answers`,
//! [`crate::answers_file`]). A digest still `PENDING` reads here (the file is whole); it is the
//! set ([`crate::BaseSet::from_row`]) that refuses it by name.

use core::fmt;

use blaze_data::Int;
use blaze_json::{Event, Limits, Reader, Str, StreamError, Writer};
use blaze_own::{AString, AVec, Alloc, String, Vec};

/// The file's path in a checkout, for messages.
pub const PATH: &str = blaze_pins::BASE_TOOLS_PATH;
/// The one form read and written here (the file's `form`).
pub const FORM: u64 = 1;
/// What a value not yet known is spelled as.
pub const PENDING: &str = "PENDING";
/// The most programs one platform's set lists (rule 18): past it the file is refused
/// ([`PinsWhy::TooMany`]). The table of D192 has 61.
pub const MOST_PROGRAMS: usize = 256;
/// The longest program, group, archive or answer text, in bytes.
pub const MOST_NAME: usize = 128;

/// A platform a base tool set is built for: the host a step runs on. Closed: a new platform is
/// a visible edit here and a row of the file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Platform {
    Aarch64AppleDarwin,
    Aarch64UnknownLinuxGnu,
    X86_64UnknownLinuxGnu,
}

impl Platform {
    /// Every platform, sorted by triple.
    pub const ALL: [Platform; 3] = [Platform::Aarch64AppleDarwin, Platform::Aarch64UnknownLinuxGnu, Platform::X86_64UnknownLinuxGnu];

    pub const fn triple(self) -> &'static str {
        match self {
            Platform::Aarch64AppleDarwin => "aarch64-apple-darwin",
            Platform::Aarch64UnknownLinuxGnu => "aarch64-unknown-linux-gnu",
            Platform::X86_64UnknownLinuxGnu => "x86_64-unknown-linux-gnu",
        }
    }

    pub fn from_triple(triple: &str) -> Option<Platform> {
        Platform::ALL.into_iter().find(|platform| platform.triple() == triple)
    }

    /// Whether a process there loads Apple's system libraries (libSystem, dyld): then every
    /// program of the set holds [`crate::FromMachine::SystemLibraries`].
    pub const fn is_apple(self) -> bool {
        matches!(self, Platform::Aarch64AppleDarwin)
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.triple())
    }
}

/// A value the file pins, or `PENDING`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pinned<T> {
    Pending,
    Is(T),
}

/// What the set's `uname` prints for each flag (`-n` is [`Answers::hostname`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Uname<'t> {
    /// `-m`, the machine (`arm64`).
    pub m: &'t str,
    /// `-o`, the operating system (`Darwin`, `GNU/Linux`).
    pub o: &'t str,
    /// `-p`, the processor (`arm`, `unknown`).
    pub p: &'t str,
    /// `-r`, the release (`20.0.0`: the deployment minimum's kernel, never the machine's).
    pub r: &'t str,
    /// `-s`, the system's name (`Darwin`).
    pub s: &'t str,
    /// `-v`, the version.
    pub v: &'t str,
}

/// The fixed answers of the set's wrappers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Answers<'t> {
    /// What `hostname` and `uname -n` print (`localhost`).
    pub hostname: &'t str,
    pub uname: Uname<'t>,
}

/// One group of a row's programs, as the file lists it.
#[derive(PartialEq, Eq, Debug)]
pub struct Group<'t, 'a> {
    pub name: &'t str,
    pub programs: AVec<'a, &'t str>,
}

/// One platform's row.
#[derive(PartialEq, Eq, Debug)]
pub struct Row<'t, 'a> {
    pub platform: Platform,
    pub archive: &'t str,
    /// The archive's SHA-256.
    pub digest: Pinned<[u8; 32]>,
    /// The archive's size in bytes: pending exactly when the digest is.
    pub size: Pinned<u64>,
    /// In the file's order.
    pub groups: AVec<'a, Group<'t, 'a>>,
    pub answers: Answers<'t>,
}

impl<'t> Row<'t, '_> {
    /// Every program of every group, sorted (each is once: the reader refuses a second).
    pub fn programs<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, &'t str> {
        let mut out: AVec<'b, &'t str> = Vec::new_in(alloc);
        for group in self.groups.iter() {
            out.extend(group.programs.iter().copied());
        }
        out.sort_unstable();
        out
    }
}

/// The whole file, read.
#[derive(PartialEq, Eq, Debug)]
pub struct PinsFile<'t, 'a> {
    /// As written, escapes and all.
    pub comment: Str<'t>,
    pub release: Pinned<&'t str>,
    /// In the file's order.
    pub rows: AVec<'a, Row<'t, 'a>>,
}

/// Why the file was refused: where in it, what was found there, and what was wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PinsError<'t> {
    /// 1-based.
    pub line: usize,
    pub column: usize,
    /// The key or name at fault, as written; empty where there is none.
    pub found: &'t str,
    pub why: PinsWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinsWhy {
    /// The text is no JSON.
    Json(blaze_json::What),
    /// `form` is not [`FORM`].
    Form,
    /// Not the schema's shape: what was wanted.
    Shape(&'static str),
    /// A key the object already has.
    TwoKeys,
    /// A key the object does not take.
    UnknownKey,
    /// A required key the object lacks.
    Missing(&'static str),
    /// A platform key that is not one of [`Platform::ALL`].
    UnknownPlatform,
    /// A program, group, archive or answer outside its form.
    BadName,
    /// A program a row lists twice.
    TwoPrograms,
    /// More than [`MOST_PROGRAMS`] programs in a row.
    TooMany,
    /// A digest neither `PENDING` nor 64 lower-case hex digits.
    Digest,
    /// `digest` and `size` not both pending or both pinned.
    HalfPinned,
}

impl fmt::Display for PinsError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PATH}:{}:{}: ", self.line, self.column)?;
        let found = self.found;
        match self.why {
            PinsWhy::Json(what) => write!(f, "{what}")?,
            PinsWhy::Form => write!(f, "`form` is not {FORM}")?,
            PinsWhy::Shape(wanted) => write!(f, "{wanted} was wanted")?,
            PinsWhy::TwoKeys => write!(f, "`{found}` is given twice")?,
            PinsWhy::UnknownKey => write!(f, "`{found}` is no key of the schema (form {FORM})")?,
            PinsWhy::Missing(key) => write!(f, "`{key}` is missing")?,
            PinsWhy::UnknownPlatform => write!(f, "`{found}` is no platform a base tool set is built for")?,
            PinsWhy::BadName => write!(f, "`{found}` is not of a name's form")?,
            PinsWhy::TwoPrograms => write!(f, "`{found}` is listed twice in one platform's programs")?,
            PinsWhy::TooMany => write!(f, "more than {MOST_PROGRAMS} programs in one platform")?,
            PinsWhy::Digest => write!(f, "`{found}` is neither {PENDING} nor 64 lower-case hex digits")?,
            PinsWhy::HalfPinned => write!(f, "`digest` and `size` must be both {PENDING} or both pinned")?,
        }
        f.write_str(" (D192)")
    }
}

impl core::error::Error for PinsError<'_> {}

impl<'t, 'a> PinsFile<'t, 'a> {
    /// The file in `text`, every row checked against the schema; held in `alloc`.
    pub fn parse(text: &'t str, alloc: Alloc<'a>) -> Result<PinsFile<'t, 'a>, PinsError<'t>> {
        let reader = Reader::new_in(text.as_bytes(), Limits::SHIPPED, alloc).map_err(|e| error_at(text, e.at, "", PinsWhy::Json(e.what)))?;
        let mut parse = Parse { reader, text, at: 0, alloc };
        let file = parse.file()?;
        match parse.reader.next() {
            Ok(None) => Ok(file),
            Ok(Some(item)) => Err(error_at(text, item.at, "", PinsWhy::Shape("the end of the text"))),
            Err(e) => Err(error_at(text, e.at, "", PinsWhy::Json(e.what))),
        }
    }

    /// The row of `platform`, if the file has one.
    pub fn row(&self, platform: Platform) -> Option<&Row<'t, 'a>> {
        self.rows.iter().find(|row| row.platform == platform)
    }

    /// The file as compact JSON in the schema's key order: what [`PinsFile::parse`] reads back
    /// equal (the round trip), and what a tool that pins a built archive writes.
    pub fn write<'b>(&self, alloc: Alloc<'b>) -> Result<AString<'b>, StreamError> {
        let mut out: AString<'b> = String::new_in(alloc);
        let mut w = Writer::new_in(&mut out, alloc);
        // A function, not a binding of `Str::plain`: each text gets its own lifetime.
        fn plain(text: &str) -> Str<'_> {
            Str::plain(text)
        }
        put(&mut w, &[Event::StartMap, Event::Key(plain("comment")), Event::Text(self.comment)])?;
        put(&mut w, &[Event::Key(plain("form")), Event::Int(Int::from_u64(FORM)), Event::Key(plain("release"))])?;
        let release = match self.release {
            Pinned::Pending => PENDING,
            Pinned::Is(url) => url,
        };
        put(&mut w, &[Event::Text(plain(release)), Event::Key(plain("platforms")), Event::StartMap])?;
        for row in self.rows.iter() {
            let mut hex = [0u8; 64];
            let digest = match &row.digest {
                Pinned::Pending => PENDING,
                Pinned::Is(digest) => hex_of(digest, &mut hex),
            };
            let size = match row.size {
                Pinned::Pending => Event::Text(plain(PENDING)),
                Pinned::Is(bytes) => Event::Int(Int::from_u64(bytes)),
            };
            put(&mut w, &[Event::Key(plain(row.platform.triple())), Event::StartMap])?;
            put(&mut w, &[Event::Key(plain("archive")), Event::Text(plain(row.archive))])?;
            put(&mut w, &[Event::Key(plain("digest")), Event::Text(plain(digest)), Event::Key(plain("size")), size])?;
            put(&mut w, &[Event::Key(plain("programs")), Event::StartMap])?;
            for group in row.groups.iter() {
                put(&mut w, &[Event::Key(plain(group.name)), Event::StartList])?;
                for program in group.programs.iter() {
                    w.event(&Event::Text(plain(program)))?;
                }
                w.event(&Event::EndList)?;
            }
            let answers = &row.answers;
            let uname = &answers.uname;
            put(&mut w, &[Event::EndMap, Event::Key(plain("answers")), Event::StartMap])?;
            put(&mut w, &[Event::Key(plain("hostname")), Event::Text(plain(answers.hostname)), Event::Key(plain("uname")), Event::StartMap])?;
            for (flag, value) in [("m", uname.m), ("o", uname.o), ("p", uname.p), ("r", uname.r), ("s", uname.s), ("v", uname.v)] {
                put(&mut w, &[Event::Key(plain(flag)), Event::Text(plain(value))])?;
            }
            put(&mut w, &[Event::EndMap, Event::EndMap, Event::EndMap])?;
        }
        put(&mut w, &[Event::EndMap, Event::EndMap])?;
        w.finish()?;
        Ok(out)
    }
}

fn put(w: &mut Writer<'_, Alloc<'_>, Alloc<'_>>, events: &[Event<'_>]) -> Result<(), StreamError> {
    for event in events {
        w.event(event)?;
    }
    Ok(())
}

/// `digest` as 64 lower-case hex digits, in `buf`.
pub(crate) fn hex_of<'b>(digest: &[u8; 32], buf: &'b mut [u8; 64]) -> &'b str {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (at, byte) in digest.iter().enumerate() {
        buf[2 * at] = DIGITS[usize::from(byte >> 4)];
        buf[2 * at + 1] = DIGITS[usize::from(byte & 0xf)];
    }
    // Hex digits are ASCII.
    core::str::from_utf8(buf).unwrap_or_default()
}

/// 64 lower-case hex digits as 32 bytes.
fn hex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = (digit(bytes[2 * at])? << 4) | digit(bytes[2 * at + 1])?;
    }
    Some(out)
}

/// A program's, group's or archive's name: `[A-Za-z0-9._+-]`, not `.` or `..`, at most
/// [`MOST_NAME`] bytes. A group's is lower-case words (`[a-z_]`).
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MOST_NAME
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

fn listed(list: &[&str], name: &str) -> bool {
    list.iter().any(|have| *have == name)
}

fn group_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= MOST_NAME && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// An answer: printable ASCII, a line of `etc/answers`.
fn answer_ok(text: &str) -> bool {
    !text.is_empty() && text.len() <= MOST_NAME && text.bytes().all(|b| (0x20..0x7f).contains(&b))
}

fn error_at<'t>(text: &'t str, at: usize, found: &'t str, why: PinsWhy) -> PinsError<'t> {
    let (line, column) = blaze_json::line_col(text.as_bytes(), at);
    PinsError { line, column, found, why }
}

/// Which of an object's keys were seen: a bit per key of its schema.
struct Seen {
    keys: &'static [&'static str],
    bits: u32,
}

impl Seen {
    const fn of(keys: &'static [&'static str]) -> Seen {
        Seen { keys, bits: 0 }
    }

    /// The first key of the schema not seen.
    fn missing(&self) -> Option<&'static str> {
        self.keys.iter().enumerate().find(|(at, _)| self.bits & (1 << at) == 0).map(|(_, key)| *key)
    }
}

struct Parse<'t, 'a> {
    reader: Reader<'t, Alloc<'a>>,
    text: &'t str,
    /// The offset of the last event read.
    at: usize,
    alloc: Alloc<'a>,
}

impl<'t, 'a> Parse<'t, 'a> {
    fn error(&self, found: &'t str, why: PinsWhy) -> PinsError<'t> {
        error_at(self.text, self.at, found, why)
    }

    fn shape(&self, wanted: &'static str) -> PinsError<'t> {
        self.error("", PinsWhy::Shape(wanted))
    }

    fn next(&mut self) -> Result<Event<'t>, PinsError<'t>> {
        match self.reader.next() {
            Ok(Some(item)) => {
                self.at = item.at;
                Ok(item.event)
            }
            Ok(None) => Err(self.shape("more text")),
            Err(e) => Err(error_at(self.text, e.at, "", PinsWhy::Json(e.what))),
        }
    }

    fn want_map(&mut self) -> Result<(), PinsError<'t>> {
        match self.next()? {
            Event::StartMap => Ok(()),
            _ => Err(self.shape("an object")),
        }
    }

    /// The next key of the open map, or `None` at its end. Keys are written without escapes.
    fn key(&mut self) -> Result<Option<&'t str>, PinsError<'t>> {
        match self.next()? {
            Event::Key(key) => key.as_plain().map(Some).ok_or_else(|| self.shape("a key without escapes")),
            Event::EndMap => Ok(None),
            _ => Err(self.shape("a key")),
        }
    }

    /// The next key of the open map, marked seen in `seen` (refused when it is no key of the
    /// schema or given twice), or `None` at the map's end (refused when a key is missing).
    fn schema_key(&mut self, seen: &mut Seen) -> Result<Option<&'static str>, PinsError<'t>> {
        let Some(key) = self.key()? else {
            return match seen.missing() {
                Some(missing) => Err(self.error("", PinsWhy::Missing(missing))),
                None => Ok(None),
            };
        };
        let Some(slot) = seen.keys.iter().position(|have| *have == key) else {
            return Err(self.error(key, PinsWhy::UnknownKey));
        };
        if seen.bits & (1 << slot) != 0 {
            return Err(self.error(key, PinsWhy::TwoKeys));
        }
        seen.bits |= 1 << slot;
        Ok(Some(seen.keys[slot]))
    }

    /// A string written without escapes.
    fn plain(&mut self, wanted: &'static str) -> Result<&'t str, PinsError<'t>> {
        match self.next()? {
            Event::Text(text) => text.as_plain().ok_or_else(|| self.shape(wanted)),
            _ => Err(self.shape(wanted)),
        }
    }

    fn file(&mut self) -> Result<PinsFile<'t, 'a>, PinsError<'t>> {
        self.want_map()?;
        let mut seen = Seen::of(&["comment", "form", "release", "platforms"]);
        let mut comment = Str::plain("");
        let mut release = Pinned::Pending;
        let mut rows: AVec<'a, Row<'t, 'a>> = Vec::new_in(self.alloc);
        while let Some(key) = self.schema_key(&mut seen)? {
            match key {
                "comment" => match self.next()? {
                    Event::Text(text) => comment = text,
                    _ => return Err(self.shape("a string")),
                },
                "form" => match self.next()? {
                    Event::Int(n) if n.as_u64() == Some(FORM) => {}
                    _ => return Err(self.error("", PinsWhy::Form)),
                },
                "release" => {
                    let url = self.plain("a URL or PENDING")?;
                    release = match url {
                        PENDING => Pinned::Pending,
                        url if url.starts_with("https://") && answer_ok(url) => Pinned::Is(url),
                        _ => return Err(self.error(url, PinsWhy::Shape("an https URL or PENDING"))),
                    };
                }
                _ => {
                    self.want_map()?;
                    while let Some(triple) = self.key()? {
                        let Some(platform) = Platform::from_triple(triple) else {
                            return Err(self.error(triple, PinsWhy::UnknownPlatform));
                        };
                        if rows.iter().any(|row: &Row<'t, 'a>| row.platform == platform) {
                            return Err(self.error(triple, PinsWhy::TwoKeys));
                        }
                        let row = self.row(platform)?;
                        rows.push(row);
                    }
                }
            }
        }
        Ok(PinsFile { comment, release, rows })
    }

    fn row(&mut self, platform: Platform) -> Result<Row<'t, 'a>, PinsError<'t>> {
        self.want_map()?;
        let mut seen = Seen::of(&["archive", "digest", "size", "programs", "answers"]);
        let (mut archive, mut digest, mut size) = ("", Pinned::Pending, Pinned::Pending);
        let mut groups: AVec<'a, Group<'t, 'a>> = Vec::new_in(self.alloc);
        let mut answers = Answers { hostname: "", uname: Uname { m: "", o: "", p: "", r: "", s: "", v: "" } };
        let mut digest_at = 0;
        while let Some(key) = self.schema_key(&mut seen)? {
            match key {
                "archive" => {
                    archive = self.plain("the archive's file name")?;
                    if !name_ok(archive) {
                        return Err(self.error(archive, PinsWhy::BadName));
                    }
                }
                "digest" => {
                    let text = self.plain("a digest or PENDING")?;
                    digest_at = self.at;
                    digest = match text {
                        PENDING => Pinned::Pending,
                        hex => Pinned::Is(hex32(hex).ok_or_else(|| self.error(hex, PinsWhy::Digest))?),
                    };
                }
                "size" => {
                    size = match self.next()? {
                        Event::Text(text) if text.as_plain() == Some(PENDING) => Pinned::Pending,
                        Event::Int(n) => match n.as_u64() {
                            Some(bytes) if bytes > 0 => Pinned::Is(bytes),
                            _ => return Err(self.shape("a size of at least one byte")),
                        },
                        _ => return Err(self.shape("a size in bytes or PENDING")),
                    };
                }
                "programs" => groups = self.groups()?,
                _ => answers = self.answers()?,
            }
        }
        if matches!(digest, Pinned::Pending) != matches!(size, Pinned::Pending) {
            return Err(error_at(self.text, digest_at, "", PinsWhy::HalfPinned));
        }
        Ok(Row { platform, archive, digest, size, groups, answers })
    }

    fn groups(&mut self) -> Result<AVec<'a, Group<'t, 'a>>, PinsError<'t>> {
        self.want_map()?;
        let mut groups: AVec<'a, Group<'t, 'a>> = Vec::new_in(self.alloc);
        let mut count = 0usize;
        while let Some(name) = self.key()? {
            if !group_ok(name) {
                return Err(self.error(name, PinsWhy::BadName));
            }
            if groups.iter().any(|group: &Group<'t, 'a>| group.name == name) {
                return Err(self.error(name, PinsWhy::TwoKeys));
            }
            match self.next()? {
                Event::StartList => {}
                _ => return Err(self.shape("a list of program names")),
            }
            let mut programs: AVec<'a, &'t str> = Vec::new_in(self.alloc);
            loop {
                let program = match self.next()? {
                    Event::EndList => break,
                    Event::Text(text) => text.as_plain().ok_or_else(|| self.shape("a program name without escapes"))?,
                    _ => return Err(self.shape("a program name")),
                };
                if !name_ok(program) {
                    return Err(self.error(program, PinsWhy::BadName));
                }
                if listed(&programs[..], program) || groups.iter().any(|group: &Group<'t, 'a>| listed(&group.programs[..], program)) {
                    return Err(self.error(program, PinsWhy::TwoPrograms));
                }
                count += 1;
                if count > MOST_PROGRAMS {
                    return Err(self.error(program, PinsWhy::TooMany));
                }
                programs.push(program);
            }
            groups.push(Group { name, programs });
        }
        Ok(groups)
    }

    fn answers(&mut self) -> Result<Answers<'t>, PinsError<'t>> {
        self.want_map()?;
        let mut seen = Seen::of(&["hostname", "uname"]);
        let mut answers = Answers { hostname: "", uname: Uname { m: "", o: "", p: "", r: "", s: "", v: "" } };
        while let Some(key) = self.schema_key(&mut seen)? {
            match key {
                "hostname" => answers.hostname = self.answer()?,
                _ => {
                    self.want_map()?;
                    let mut flags = Seen::of(&["m", "o", "p", "r", "s", "v"]);
                    while let Some(flag) = self.schema_key(&mut flags)? {
                        let value = self.answer()?;
                        let uname = &mut answers.uname;
                        match flag {
                            "m" => uname.m = value,
                            "o" => uname.o = value,
                            "p" => uname.p = value,
                            "r" => uname.r = value,
                            "s" => uname.s = value,
                            _ => uname.v = value,
                        }
                    }
                }
            }
        }
        Ok(answers)
    }

    fn answer(&mut self) -> Result<&'t str, PinsError<'t>> {
        let text = self.plain("an answer without escapes")?;
        if !answer_ok(text) {
            return Err(self.error(text, PinsWhy::BadName));
        }
        Ok(text)
    }
}
