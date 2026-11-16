//! The outputs of one run, as one record: the instructions parsed into typed values and the files
//! the script wrote into `OUT_DIR`.
//!
//! `rerun-if-changed` and `rerun-if-env-changed` are read and dropped: they are advice for a
//! builder that cannot see what a script reads, and the sandbox records every real read (the
//! record's read list). Everything else is kept in the order printed; the files sorted by name.
//!
//! The bytes, every number little-endian, a string a `u32` length then UTF-8:
//!
//! ```text
//! u64  LAYOUT
//! out-dir     the `OUT_DIR` the script ran with
//! then twelve sections, each a u32 count and its entries:
//!   cfgs        name, u8 has-value, [value]
//!   check-cfgs  spec
//!   env         name, value
//!   link-libs   lib
//!   link-search u8 kind, path
//!   link-args   u8 target, bin (empty but for `rustc-link-arg-bin`), arg
//!   metadata    key, value
//!   warnings    message
//!   errors      message
//!   notes       u32 line, u8 what (0 an unknown `cargo::` key, else 1 + `Malformed`), key
//!   files       name, u64 length, bytes
//!   c-requests  argv (the program first), joined by NUL
//! ```
//!
//! The `OUT_DIR` is kept because a script may print a path into it (mime_guess's
//! `rustc-env=MIME_TYPES_GENERATED_PATH=<OUT_DIR>/mime_types_generated.rs`, read by
//! `include!(env!(..))`): the crate's `env!("OUT_DIR")` and every `include!` under it must name
//! the very folder the script saw. It is a path of the sandbox, a function of the unit (no host
//! path), and is in the run's key through the environment.
//!
//! A C request is a spawn of a C toolchain program (`cc`, `ar`, `xcrun`, ...) that a `check`
//! run deferred ([`crate::key::CMode::Deferred`]): the script was told it succeeded with no
//! output, and the file it would have made was not made. The C mode is in the key, so a
//! `build` never takes a record that holds one.
//!
//! [`OutputsRef::open`] checks the whole once and reads in place; [`Outputs::encode`] of what
//! [`OutputsRef::to_owned`] gives is the same bytes (tested).

#![blaze::sans_io]

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::instr::{self, Instr, Line, LinkTo, Malformed, OutputError, OutputLimits, SearchKind};

/// The record's layout word: `bzbsout` and the layout's number.
pub const LAYOUT: u64 = u64::from_le_bytes(*b"bzbsout3");

/// A link argument as kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkArg<'a> {
    /// [`LinkTo::code`].
    pub to: u8,
    /// The binary of `rustc-link-arg-bin`, else empty.
    pub bin: AString<'a>,
    pub arg: AString<'a>,
}

/// A line that was read and not kept as an instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoteKind {
    /// A `cargo::` key Cargo does not document.
    Unknown,
    Malformed(Malformed),
}

impl NoteKind {
    fn code(self) -> u8 {
        match self {
            NoteKind::Unknown => 0,
            NoteKind::Malformed(why) => 1 + why as u8,
        }
    }

    fn from_code(code: u8) -> Option<NoteKind> {
        use Malformed::*;
        const ALL: [Malformed; 9] = [NoValue, NotUtf8, Cfg, Env, LinkSearch, LinkLib, LinkArgBin, Metadata, Flags];
        match code {
            0 => Some(NoteKind::Unknown),
            n => ALL.get(usize::from(n) - 1).copied().map(NoteKind::Malformed),
        }
    }
}

/// A typed warning about one line of output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note<'a> {
    /// From 1.
    pub line: u32,
    pub kind: NoteKind,
    pub key: AString<'a>,
}

/// One file the script wrote into `OUT_DIR`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutFile<'a> {
    /// Relative to `OUT_DIR`, `/`-separated.
    pub name: AString<'a>,
    pub bytes: AVec<'a, u8>,
}

/// What one run gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outputs<'a> {
    /// The `OUT_DIR` the script ran with: an absolute path of the sandbox, never empty.
    pub out_dir: AString<'a>,
    pub cfgs: AVec<'a, (AString<'a>, Option<AString<'a>>)>,
    pub check_cfgs: AVec<'a, AString<'a>>,
    pub env: AVec<'a, (AString<'a>, AString<'a>)>,
    pub link_libs: AVec<'a, AString<'a>>,
    pub link_search: AVec<'a, (SearchKind, AString<'a>)>,
    pub link_args: AVec<'a, LinkArg<'a>>,
    pub metadata: AVec<'a, (AString<'a>, AString<'a>)>,
    pub warnings: AVec<'a, AString<'a>>,
    pub errors: AVec<'a, AString<'a>>,
    pub notes: AVec<'a, Note<'a>>,
    /// Sorted by name.
    pub files: AVec<'a, OutFile<'a>>,
    /// The native tool spawns a `check` run deferred (the C toolchain, `perl`, `make`, a
    /// `configure`: `probe::is_deferred_tool`): each argv joined by NUL, in spawn order.
    pub c_requests: AVec<'a, AString<'a>>,
}

/// How much a script may write into `OUT_DIR`.
#[derive(Clone, Copy, Debug)]
pub struct FileLimits {
    /// All files' bytes together.
    pub bytes: u64,
    pub files: usize,
}

impl FileLimits {
    /// 64 MiB and 4096 files.
    pub const DEFAULT: FileLimits = FileLimits { bytes: 64 << 20, files: 4096 };
}

/// Why a file is not taken into the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileError {
    /// The files would be `bytes` in all, over `limit`.
    TooLarge { bytes: u64, limit: u64 },
    /// More than `limit` files.
    TooMany { limit: usize },
    /// A name that is empty, absolute, holds `.` or `..` or an empty part: it would leave
    /// `OUT_DIR` or name one file two ways.
    BadName,
    /// The same name twice.
    Twice,
}

fn own<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    String::from_str_in(text, alloc)
}

impl<'a> Outputs<'a> {
    /// No outputs yet of a script run with `OUT_DIR` at `out_dir`.
    pub fn new_in(out_dir: &str, alloc: Alloc<'a>) -> Outputs<'a> {
        Outputs {
            out_dir: own(out_dir, alloc),
            cfgs: Vec::new_in(alloc),
            check_cfgs: Vec::new_in(alloc),
            env: Vec::new_in(alloc),
            link_libs: Vec::new_in(alloc),
            link_search: Vec::new_in(alloc),
            link_args: Vec::new_in(alloc),
            metadata: Vec::new_in(alloc),
            warnings: Vec::new_in(alloc),
            errors: Vec::new_in(alloc),
            notes: Vec::new_in(alloc),
            files: Vec::new_in(alloc),
            c_requests: Vec::new_in(alloc),
        }
    }

    /// The instructions of a script's whole standard output, run with `OUT_DIR` at `out_dir`.
    pub fn read(output: &[u8], out_dir: &str, limits: OutputLimits, alloc: Alloc<'a>) -> Result<Outputs<'a>, OutputError> {
        let mut out = Outputs::new_in(out_dir, alloc);
        for (at, line) in instr::lines(output, limits)? {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            match line {
                Line::Plain => {}
                Line::Instr(_, instr) => out.take(instr, alloc),
                Line::Flags(_, value) => {
                    for flag in instr::flags(value) {
                        match flag {
                            Ok(instr) => out.take(instr, alloc),
                            Err(why) => out.notes.push(Note { line: at, kind: NoteKind::Malformed(why), key: own("rustc-flags", alloc) }),
                        }
                    }
                }
                Line::Unknown { key } => out.notes.push(Note { line: at, kind: NoteKind::Unknown, key: own(key, alloc) }),
                Line::Malformed { key, why } => out.notes.push(Note { line: at, kind: NoteKind::Malformed(why), key: own(key, alloc) }),
            }
        }
        Ok(out)
    }

    fn take(&mut self, instr: Instr<'_>, alloc: Alloc<'a>) {
        match instr {
            Instr::Cfg { name, value } => self.cfgs.push((own(name, alloc), value.map(|v| own(v, alloc)))),
            Instr::CheckCfg(spec) => self.check_cfgs.push(own(spec, alloc)),
            Instr::Env { name, value } => self.env.push((own(name, alloc), own(value, alloc))),
            Instr::LinkLib(lib) => self.link_libs.push(own(lib, alloc)),
            Instr::LinkSearch { kind, path } => self.link_search.push((kind, own(path, alloc))),
            Instr::LinkArg { to, arg } => {
                let bin = match to {
                    LinkTo::Bin(bin) => bin,
                    _ => "",
                };
                self.link_args.push(LinkArg { to: to.code(), bin: own(bin, alloc), arg: own(arg, alloc) })
            }
            Instr::Metadata { key, value } => self.metadata.push((own(key, alloc), own(value, alloc))),
            Instr::RerunIfChanged(_) | Instr::RerunIfEnvChanged(_) => {}
            Instr::Warning(message) => self.warnings.push(own(message, alloc)),
            Instr::Error(message) => self.errors.push(own(message, alloc)),
        }
    }

    /// Takes a file the script left in `OUT_DIR`, kept sorted by name, within `limits`.
    pub fn add_file(&mut self, name: &str, bytes: &[u8], limits: FileLimits, alloc: Alloc<'a>) -> Result<(), FileError> {
        let bad = name.is_empty() || name.starts_with('/') || name.split('/').any(|part| matches!(part, "" | "." | ".."));
        if bad {
            return Err(FileError::BadName);
        }
        if self.files.len() >= limits.files {
            return Err(FileError::TooMany { limit: limits.files });
        }
        let total = self.files.iter().map(|f| f.bytes.len() as u64).sum::<u64>() + bytes.len() as u64;
        if total > limits.bytes {
            return Err(FileError::TooLarge { bytes: total, limit: limits.bytes });
        }
        let at = match self.files.binary_search_by(|f| f.name.as_str().cmp(name)) {
            Ok(_) => return Err(FileError::Twice),
            Err(at) => at,
        };
        let mut owned = Vec::with_capacity_in(bytes.len(), alloc);
        owned.extend_from_slice(bytes);
        self.files.insert(at, OutFile { name: own(name, alloc), bytes: owned });
        Ok(())
    }

    /// The file `name` (relative to `OUT_DIR`), as the crate's `include!` reads it.
    pub fn file(&self, name: &str) -> Option<&[u8]> {
        let at = self.files.binary_search_by(|f| f.name.as_str().cmp(name)).ok()?;
        Some(&self.files[at].bytes)
    }

    /// Appends the record's bytes to `out`.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        out.extend_from_slice(&LAYOUT.to_le_bytes());
        text(out, &self.out_dir);
        count(out, self.cfgs.len());
        for (name, value) in self.cfgs.iter() {
            text(out, name);
            match value {
                Some(value) => {
                    out.push(1);
                    text(out, value);
                }
                None => out.push(0),
            }
        }
        count(out, self.check_cfgs.len());
        self.check_cfgs.iter().for_each(|it| text(out, it));
        pairs(out, &self.env);
        count(out, self.link_libs.len());
        self.link_libs.iter().for_each(|it| text(out, it));
        count(out, self.link_search.len());
        for (kind, path) in self.link_search.iter() {
            out.push(*kind as u8);
            text(out, path);
        }
        count(out, self.link_args.len());
        for arg in self.link_args.iter() {
            out.push(arg.to);
            text(out, &arg.bin);
            text(out, &arg.arg);
        }
        pairs(out, &self.metadata);
        count(out, self.warnings.len());
        self.warnings.iter().for_each(|it| text(out, it));
        count(out, self.errors.len());
        self.errors.iter().for_each(|it| text(out, it));
        count(out, self.notes.len());
        for note in self.notes.iter() {
            out.extend_from_slice(&note.line.to_le_bytes());
            out.push(note.kind.code());
            text(out, &note.key);
        }
        count(out, self.files.len());
        for file in self.files.iter() {
            text(out, &file.name);
            out.extend_from_slice(&(file.bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(&file.bytes);
        }
        count(out, self.c_requests.len());
        self.c_requests.iter().for_each(|it| text(out, it));
    }
}

fn count(out: &mut AVec<'_, u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
}

fn text(out: &mut AVec<'_, u8>, text: &str) {
    count(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

fn pairs(out: &mut AVec<'_, u8>, pairs: &[(AString<'_>, AString<'_>)]) {
    count(out, pairs.len());
    for (a, b) in pairs {
        text(out, a);
        text(out, b);
    }
}

/// Why bytes are not an outputs record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordError {
    /// The layout word is not [`LAYOUT`].
    Layout,
    /// The bytes end inside an entry at `at`.
    Short { at: usize },
    /// A string at `at` is not UTF-8.
    NotUtf8 { at: usize },
    /// A kind byte at `at` is none this layout has.
    Kind { at: usize },
    /// Bytes are left after the last section.
    Trailing { at: usize },
    /// The `OUT_DIR` at `at` is not an absolute path.
    OutDir { at: usize },
}

/// A cursor over a record's bytes.
#[derive(Clone, Copy)]
struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], RecordError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(RecordError::Short { at: self.at })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, RecordError> {
        let mut word = [0; 4];
        word.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(word))
    }

    fn u64(&mut self) -> Result<u64, RecordError> {
        let mut word = [0; 8];
        word.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(word))
    }

    fn text(&mut self) -> Result<&'b str, RecordError> {
        let at = self.at;
        let len = self.u32()? as usize;
        core::str::from_utf8(self.take(len)?).map_err(|_| RecordError::NotUtf8 { at })
    }

    fn blob(&mut self) -> Result<&'b [u8], RecordError> {
        let at = self.at;
        let len = usize::try_from(self.u64()?).map_err(|_| RecordError::Short { at })?;
        self.take(len)
    }
}

/// The twelve sections' starts (after each count) and counts.
const SECTIONS: usize = 12;

/// The domain of [`OutputsRef::crate_digest`]; a change to what it covers takes the next number.
pub const CRATE_VIEW: &str = "blaze v2 build-script crate view 1";

/// An outputs record, read in place.
#[derive(Clone, Copy)]
pub struct OutputsRef<'b> {
    bytes: &'b [u8],
    out_dir: &'b str,
    starts: [(usize, u32); SECTIONS],
}

/// One entry of any section, borrowed.
enum Entry<'b> {
    Cfg(&'b str, Option<&'b str>),
    Text(&'b str),
    Pair(&'b str, &'b str),
    Search(SearchKind, &'b str),
    Arg(u8, &'b str, &'b str),
    Note(u32, NoteKind, &'b str),
    File(&'b str, &'b [u8]),
}

fn entry<'b>(section: usize, cur: &mut Cursor<'b>) -> Result<Entry<'b>, RecordError> {
    Ok(match section {
        0 => {
            let name = cur.text()?;
            let at = cur.at;
            match cur.u8()? {
                0 => Entry::Cfg(name, None),
                1 => Entry::Cfg(name, Some(cur.text()?)),
                _ => return Err(RecordError::Kind { at }),
            }
        }
        1 | 3 | 7 | 8 | 11 => Entry::Text(cur.text()?),
        2 | 6 => Entry::Pair(cur.text()?, cur.text()?),
        4 => {
            let at = cur.at;
            let kind = SearchKind::from_u8(cur.u8()?).ok_or(RecordError::Kind { at })?;
            Entry::Search(kind, cur.text()?)
        }
        5 => {
            let at = cur.at;
            let to = cur.u8()?;
            if to > 6 {
                return Err(RecordError::Kind { at });
            }
            Entry::Arg(to, cur.text()?, cur.text()?)
        }
        9 => {
            let line = cur.u32()?;
            let at = cur.at;
            let kind = NoteKind::from_code(cur.u8()?).ok_or(RecordError::Kind { at })?;
            Entry::Note(line, kind, cur.text()?)
        }
        _ => Entry::File(cur.text()?, cur.blob()?),
    })
}

impl<'b> OutputsRef<'b> {
    /// Checks every entry of `bytes` once.
    pub fn open(bytes: &'b [u8]) -> Result<OutputsRef<'b>, RecordError> {
        let mut cur = Cursor { bytes, at: 0 };
        if cur.u64().ok() != Some(LAYOUT) {
            return Err(RecordError::Layout);
        }
        let at = cur.at;
        let out_dir = cur.text()?;
        if !out_dir.starts_with('/') {
            return Err(RecordError::OutDir { at });
        }
        let mut starts = [(0, 0); SECTIONS];
        for (section, start) in starts.iter_mut().enumerate() {
            let n = cur.u32()?;
            *start = (cur.at, n);
            for _ in 0..n {
                entry(section, &mut cur)?;
            }
        }
        if cur.at != bytes.len() {
            return Err(RecordError::Trailing { at: cur.at });
        }
        Ok(OutputsRef { bytes, out_dir, starts })
    }

    /// The `OUT_DIR` the script ran with.
    pub fn out_dir(&self) -> &'b str {
        self.out_dir
    }

    /// The digest of everything of the record the served crate's text can read: its `OUT_DIR`,
    /// its cfgs, its env pairs and its files (names and bytes). A crate's key covers this, so
    /// two records that differ in any of these are two keys; what the crate cannot read (link
    /// requests, metadata, warnings, C requests) is left out.
    pub fn crate_digest(&self) -> blaze_ids::ContentId {
        let mut hasher = blaze_ids::Hasher::new(CRATE_VIEW);
        hasher.part(self.out_dir.as_bytes());
        for section in [0, 2, 10] {
            let (start, n) = self.starts[section];
            // A section's bytes run to the next section's count.
            let end = self.starts[section + 1].0 - 4;
            hasher.update(&n.to_le_bytes()).part(&self.bytes[start..end]);
        }
        hasher.finish()
    }

    /// The file a crate names by `path` when it is under the script's `OUT_DIR`: `Some` of the
    /// record's bytes, or `Some(None)` when the record has no such file; `None` when `path` is
    /// not under `OUT_DIR` at all.
    pub fn file_at(&self, path: &str) -> Option<Option<&'b [u8]>> {
        let name = path.strip_prefix(self.out_dir)?.strip_prefix('/')?;
        Some(self.file(name))
    }

    /// The entries of `section`, which `open` checked.
    fn entries(&self, section: usize) -> impl Iterator<Item = Entry<'b>> + use<'b> {
        let (at, n) = self.starts[section];
        let mut cur = Cursor { bytes: self.bytes, at };
        (0..n).map_while(move |_| entry(section, &mut cur).ok())
    }

    pub fn cfgs(&self) -> impl Iterator<Item = (&'b str, Option<&'b str>)> + use<'b> {
        self.entries(0).filter_map(|e| if let Entry::Cfg(n, v) = e { Some((n, v)) } else { None })
    }

    fn texts(&self, section: usize) -> impl Iterator<Item = &'b str> + use<'b> {
        self.entries(section).filter_map(|e| if let Entry::Text(t) = e { Some(t) } else { None })
    }

    fn pairs(&self, section: usize) -> impl Iterator<Item = (&'b str, &'b str)> + use<'b> {
        self.entries(section).filter_map(|e| if let Entry::Pair(a, b) = e { Some((a, b)) } else { None })
    }

    pub fn check_cfgs(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(1)
    }

    pub fn env(&self) -> impl Iterator<Item = (&'b str, &'b str)> + use<'b> {
        self.pairs(2)
    }

    pub fn link_libs(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(3)
    }

    pub fn link_search(&self) -> impl Iterator<Item = (SearchKind, &'b str)> + use<'b> {
        self.entries(4).filter_map(|e| if let Entry::Search(k, p) = e { Some((k, p)) } else { None })
    }

    /// (target code, bin, arg).
    pub fn link_args(&self) -> impl Iterator<Item = (u8, &'b str, &'b str)> + use<'b> {
        self.entries(5).filter_map(|e| if let Entry::Arg(t, b, a) = e { Some((t, b, a)) } else { None })
    }

    pub fn metadata(&self) -> impl Iterator<Item = (&'b str, &'b str)> + use<'b> {
        self.pairs(6)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(7)
    }

    pub fn errors(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(8)
    }

    pub fn notes(&self) -> impl Iterator<Item = (u32, NoteKind, &'b str)> + use<'b> {
        self.entries(9).filter_map(|e| if let Entry::Note(l, k, t) = e { Some((l, k, t)) } else { None })
    }

    /// (name, bytes), sorted by name.
    pub fn files(&self) -> impl Iterator<Item = (&'b str, &'b [u8])> + use<'b> {
        self.entries(10).filter_map(|e| if let Entry::File(n, b) = e { Some((n, b)) } else { None })
    }

    /// The file `name`, by a scan of the sorted names.
    pub fn file(&self, name: &str) -> Option<&'b [u8]> {
        self.files().find(|(n, _)| *n == name).map(|(_, b)| b)
    }

    /// The deferred C requests, each argv joined by NUL.
    pub fn c_requests(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(11)
    }

    /// The record owned in `alloc`.
    pub fn to_owned<'a>(&self, alloc: Alloc<'a>) -> Outputs<'a> {
        let mut out = Outputs::new_in(self.out_dir, alloc);
        for (name, value) in self.cfgs() {
            out.cfgs.push((own(name, alloc), value.map(|v| own(v, alloc))));
        }
        self.check_cfgs().for_each(|it| out.check_cfgs.push(own(it, alloc)));
        self.env().for_each(|(a, b)| out.env.push((own(a, alloc), own(b, alloc))));
        self.link_libs().for_each(|it| out.link_libs.push(own(it, alloc)));
        self.link_search().for_each(|(k, p)| out.link_search.push((k, own(p, alloc))));
        self.link_args().for_each(|(to, bin, arg)| out.link_args.push(LinkArg { to, bin: own(bin, alloc), arg: own(arg, alloc) }));
        self.metadata().for_each(|(a, b)| out.metadata.push((own(a, alloc), own(b, alloc))));
        self.warnings().for_each(|it| out.warnings.push(own(it, alloc)));
        self.errors().for_each(|it| out.errors.push(own(it, alloc)));
        self.notes().for_each(|(line, kind, key)| out.notes.push(Note { line, kind, key: own(key, alloc) }));
        for (name, bytes) in self.files() {
            let mut owned = Vec::with_capacity_in(bytes.len(), alloc);
            owned.extend_from_slice(bytes);
            out.files.push(OutFile { name: own(name, alloc), bytes: owned });
        }
        self.c_requests().for_each(|it| out.c_requests.push(own(it, alloc)));
        out
    }
}
