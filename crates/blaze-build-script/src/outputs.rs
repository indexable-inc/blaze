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
//! then the stored files, a u32 count and its entries:
//!   stored      name, u64 length, u32 mode (`0o100644` or `0o100755`), 32 bytes SHA-256
//! ```
//!
//! A stored file is one the record names and does not hold: a native script's `OUT_DIR` file
//! (a C library's `.a` is hundreds of MB: librocksdb-sys's is 810 MB), whose bytes went from
//! the script's folder to the home's file area ([`FILE_AREA`], one file per digest) by a copy
//! in fixed chunks, never through the command's memory, its digest streamed the same way. A
//! reader that needs one stored file's bytes (`include!` of a generated `bindings.rs`, a link
//! of the `.a`) asks for that one file ([`OutputsRef::bytes_of`]), mapped read-only by the
//! command that opened the record ([`StoredBytes`]); nothing reads a stored file it does not
//! name, so `check` never touches a `.a`. No limit is on a stored file's size: the count of
//! files is the limit ([`FileLimits::files`]). Names are sorted and the mode is whether an
//! execute bit is set, alone (the rest of a mode is the machine's umask), so two runs of one key give
//! the same bytes (rule 25). A wasm script's files are in its sandbox's memory and stay inline
//! (`files`); one name is never in both sections.
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

/// The record's layout word: `bzbsout` and the layout's number. `bzbsout5` added the stored
/// files; a `bzbsout3` record is another layout ([`RecordError::Layout`]) and the run key
/// holds this word (`key.rs`), so an old home's records are never found and are made again
/// (rule 26).
pub const LAYOUT: u64 = u64::from_le_bytes(*b"bzbsout5");

/// The home's file area, under `<home>/.blaze/v2`: each stored file at `<this>/<sha256 hex>`,
/// placed once by whoever ran the script and read by every command that opens its record.
pub const FILE_AREA: &str = "build-script/files";

/// A stored file's mode as the record keeps it: whether an execute bit is set, nothing of
/// the machine's umask (rule 25).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FileMode {
    /// `0o100644`.
    Plain,
    /// `0o100755`.
    Executable,
}

impl FileMode {
    /// The mode word the record holds.
    pub const fn word(self) -> u32 {
        match self {
            FileMode::Plain => 0o100644,
            FileMode::Executable => 0o100755,
        }
    }

    /// Of a mode word the record holds; any other word is none.
    pub const fn of_word(word: u32) -> Option<FileMode> {
        match word {
            0o100644 => Some(FileMode::Plain),
            0o100755 => Some(FileMode::Executable),
            _ => None,
        }
    }
}

/// One file the record names and does not hold: its bytes are in the [`FILE_AREA`] under
/// `sha256`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFile<'a> {
    /// Relative to `OUT_DIR`, `/`-separated.
    pub name: AString<'a>,
    pub len: u64,
    pub mode: FileMode,
    pub sha256: [u8; 32],
}

/// A stored file as a record read in place names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredRef<'b> {
    pub name: &'b str,
    pub len: u64,
    pub mode: FileMode,
    pub sha256: &'b [u8; 32],
}

/// One file of a record read in place: its bytes held in the record, or named by digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutFileRef<'b> {
    Inline(&'b [u8]),
    Stored(StoredRef<'b>),
}

/// Where the command that opened a record reads its stored files' bytes: the home's
/// [`FILE_AREA`], each file mapped read-only on its first ask (blaze-cli `scripts::FileArea`),
/// or a test's files.
pub trait StoredBytes: Sync {
    /// The bytes of `file`, held as long as `self` is: exactly `file.len` of them, or why not.
    fn bytes(&self, file: &StoredRef<'_>) -> Result<&[u8], StoredError>;
}

/// Why a stored file's bytes were not read; the caller names the file
/// ([`StoredRef::name`]), this its digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoredError {
    pub sha256: [u8; 32],
    pub why: StoredWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoredWhy {
    /// The record was opened with no file area ([`OutputsRef::with_area`]): the reader asked
    /// a stored file it cannot read; nothing is answered in its place (rule 3).
    NoArea,
    /// The file area has no file of this digest.
    NotThere,
    /// The file area's file of this digest has `found` bytes, not the record's length.
    Length { found: u64 },
    /// The file area's file could not be mapped.
    Unreadable,
}

impl core::fmt::Display for StoredError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the stored file ")?;
        for byte in self.sha256 {
            write!(f, "{byte:02x}")?;
        }
        match self.why {
            StoredWhy::NoArea => write!(f, " was asked of a record opened with no file area ({FILE_AREA}); it is not read in place of one (rule 3)"),
            StoredWhy::NotThere => write!(f, " is not in the file area ({FILE_AREA}): the record that names it was made by a home that kept it"),
            StoredWhy::Length { found } => write!(f, " has {found} bytes in the file area ({FILE_AREA}), not the record's length"),
            StoredWhy::Unreadable => write!(f, " could not be mapped from the file area ({FILE_AREA})"),
        }
    }
}

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
    /// Sorted by name; no name of `files` among them.
    pub stored: AVec<'a, StoredFile<'a>>,
}

/// How much a script may write into `OUT_DIR`.
#[derive(Clone, Copy, Debug)]
pub struct FileLimits {
    /// All inline files' bytes together (a wasm script's, held in its sandbox's memory); a
    /// stored file's length is under no limit.
    pub bytes: u64,
    /// Inline and stored files together.
    pub files: usize,
}

impl FileLimits {
    /// 1 GiB and 4096 files: a file over [`crate::outputs4::BLOB_OVER`] is its own record in
    /// `bzbsout4` (NATIVE.md 3.8), so the total no longer sizes one record.
    pub const DEFAULT: FileLimits = FileLimits { bytes: 1 << 30, files: 4096 };
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
            stored: Vec::new_in(alloc),
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
        self.room_for(name, limits)?;
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

    /// Takes a file the script left in `OUT_DIR` whose bytes are in the [`FILE_AREA`] under
    /// `sha256`, kept sorted by name; only the count of files is limited, never its length.
    pub fn add_stored(&mut self, name: &str, len: u64, mode: FileMode, sha256: [u8; 32], limits: FileLimits, alloc: Alloc<'a>) -> Result<(), FileError> {
        self.room_for(name, limits)?;
        let at = match self.stored.binary_search_by(|f| f.name.as_str().cmp(name)) {
            Ok(_) => return Err(FileError::Twice),
            Err(at) => at,
        };
        self.stored.insert(at, StoredFile { name: own(name, alloc), len, mode, sha256 });
        Ok(())
    }

    /// That `name` is a name a record may hold, held by neither section yet, with room for
    /// one more file.
    fn room_for(&self, name: &str, limits: FileLimits) -> Result<(), FileError> {
        let bad = name.is_empty() || name.starts_with('/') || name.split('/').any(|part| matches!(part, "" | "." | ".."));
        if bad {
            return Err(FileError::BadName);
        }
        if self.files.len() + self.stored.len() >= limits.files {
            return Err(FileError::TooMany { limit: limits.files });
        }
        let inline = self.files.binary_search_by(|f| f.name.as_str().cmp(name)).is_ok();
        let stored = self.stored.binary_search_by(|f| f.name.as_str().cmp(name)).is_ok();
        if inline || stored {
            return Err(FileError::Twice);
        }
        Ok(())
    }

    /// The inline file `name` (relative to `OUT_DIR`); a stored one is [`Outputs::stored_file`].
    pub fn file(&self, name: &str) -> Option<&[u8]> {
        let at = self.files.binary_search_by(|f| f.name.as_str().cmp(name)).ok()?;
        Some(&self.files[at].bytes)
    }

    /// The stored file `name` (relative to `OUT_DIR`).
    pub fn stored_file(&self, name: &str) -> Option<&StoredFile<'a>> {
        let at = self.stored.binary_search_by(|f| f.name.as_str().cmp(name)).ok()?;
        Some(&self.stored[at])
    }

    /// Appends the record's bytes to `out`.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        self.encode_head(LAYOUT, out);
        count(out, self.files.len());
        for file in self.files.iter() {
            text(out, &file.name);
            out.extend_from_slice(&(file.bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(&file.bytes);
        }
        self.encode_tail(out);
        count(out, self.stored.len());
        for file in self.stored.iter() {
            text(out, &file.name);
            out.extend_from_slice(&file.len.to_le_bytes());
            out.extend_from_slice(&file.mode.word().to_le_bytes());
            out.extend_from_slice(&file.sha256);
        }
    }

    /// The layout word `layout`, the `OUT_DIR` and the ten sections before the files: one
    /// encoding for every layout (`bzbsout4` in [`crate::outputs4`] shares it).
    pub(crate) fn encode_head(&self, layout: u64, out: &mut AVec<'_, u8>) {
        out.extend_from_slice(&layout.to_le_bytes());
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
    }

    /// The section after the files: the C requests.
    pub(crate) fn encode_tail(&self, out: &mut AVec<'_, u8>) {
        count(out, self.c_requests.len());
        self.c_requests.iter().for_each(|it| text(out, it));
    }
}

pub(crate) fn count(out: &mut AVec<'_, u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
}

pub(crate) fn text(out: &mut AVec<'_, u8>, text: &str) {
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
    /// The record is of the earlier layout `tag` (`bzbsout3` read by
    /// [`crate::outputs4::Outputs4Ref::open`]): an old home, reported and rebuilt (rule 26),
    /// never read.
    OldHome { tag: u64 },
    /// A `bzbsout4` file's form byte at `at` disagrees with its length: inline over
    /// [`crate::outputs4::BLOB_OVER`] or a blob not over it (one outputs, one encoding).
    FileForm { at: usize },
}

/// A cursor over a record's bytes.
#[derive(Clone, Copy)]
pub(crate) struct Cursor<'b> {
    pub(crate) bytes: &'b [u8],
    pub(crate) at: usize,
}

impl<'b> Cursor<'b> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'b [u8], RecordError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(RecordError::Short { at: self.at })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, RecordError> {
        let mut word = [0; 4];
        word.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(word))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, RecordError> {
        let mut word = [0; 8];
        word.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(word))
    }

    pub(crate) fn text(&mut self) -> Result<&'b str, RecordError> {
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
pub(crate) const SECTIONS: usize = 12;

/// The domain of [`OutputsRef::crate_digest`]; a change to what it covers takes the next number.
pub const CRATE_VIEW: &str = "blaze v2 build-script crate view 2";

/// An outputs record, read in place.
#[derive(Clone, Copy)]
pub struct OutputsRef<'b> {
    bytes: &'b [u8],
    out_dir: &'b str,
    starts: [(usize, u32); SECTIONS],
    /// The stored files' section: its start (after its count) and count.
    stored_at: (usize, u32),
    /// Where its stored files' bytes are read; `None` until [`OutputsRef::with_area`].
    area: Option<&'b dyn StoredBytes>,
}

/// One stored file's entry, checked.
fn stored_entry<'b>(cur: &mut Cursor<'b>) -> Result<StoredRef<'b>, RecordError> {
    let name = cur.text()?;
    let len = cur.u64()?;
    let at = cur.at;
    let mode = FileMode::of_word(cur.u32()?).ok_or(RecordError::Kind { at })?;
    let sha256 = <&[u8; 32]>::try_from(cur.take(32)?).map_err(|_| RecordError::Short { at: cur.at })?;
    Ok(StoredRef { name, len, mode, sha256 })
}

/// One entry of any section, borrowed.
pub(crate) enum Entry<'b> {
    Cfg(&'b str, Option<&'b str>),
    Text(&'b str),
    Pair(&'b str, &'b str),
    Search(SearchKind, &'b str),
    Arg(u8, &'b str, &'b str),
    Note(u32, NoteKind, &'b str),
    File(&'b str, &'b [u8]),
}

pub(crate) fn entry<'b>(section: usize, cur: &mut Cursor<'b>) -> Result<Entry<'b>, RecordError> {
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
        let n = cur.u32()?;
        let stored_at = (cur.at, n);
        for _ in 0..n {
            stored_entry(&mut cur)?;
        }
        if cur.at != bytes.len() {
            return Err(RecordError::Trailing { at: cur.at });
        }
        Ok(OutputsRef { bytes, out_dir, starts, stored_at, area: None })
    }

    /// This record with its stored files read from `area`.
    pub fn with_area(self, area: &'b dyn StoredBytes) -> OutputsRef<'b> {
        OutputsRef { area: Some(area), ..self }
    }

    /// Where its stored files are read from, if one was attached ([`OutputsRef::with_area`]): a
    /// reader that copies the record's bytes and opens them again attaches it again.
    pub fn area(&self) -> Option<&'b dyn StoredBytes> {
        self.area
    }

    /// The `OUT_DIR` the script ran with.
    pub fn out_dir(&self) -> &'b str {
        self.out_dir
    }

    /// The record's bytes, as [`OutputsRef::open`] took them: a reader that must outlive this
    /// borrow copies them into its own frame and opens them again.
    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    /// The digest of everything of the record the served crate's text can read: its `OUT_DIR`,
    /// its cfgs, its env pairs and its files (names and bytes; a stored file's name, length,
    /// mode and digest). A crate's key covers this, so two records that differ in any of these
    /// are two keys; what the crate cannot read (link requests, metadata, warnings, C
    /// requests) is left out.
    pub fn crate_digest(&self) -> blaze_ids::ContentId {
        let mut hasher = blaze_ids::Hasher::new(CRATE_VIEW);
        hasher.part(self.out_dir.as_bytes());
        for section in [0, 2, 10] {
            let (start, n) = self.starts[section];
            // A section's bytes run to the next section's count.
            let end = self.starts[section + 1].0 - 4;
            hasher.update(&n.to_le_bytes()).part(&self.bytes[start..end]);
        }
        // The stored files' section runs to the record's end.
        let (start, n) = self.stored_at;
        hasher.update(&n.to_le_bytes()).part(&self.bytes[start..]);
        hasher.finish()
    }

    /// The file a crate names by `path` when it is under the script's `OUT_DIR`: `Some` of the
    /// record's file, or `Some(None)` when the record has no such file; `None` when `path` is
    /// not under `OUT_DIR` at all.
    pub fn file_at(&self, path: &str) -> Option<Option<OutFileRef<'b>>> {
        let name = path.strip_prefix(self.out_dir)?.strip_prefix('/')?;
        Some(self.file(name))
    }

    /// The bytes of one of the record's files: an inline file's in place, a stored file's
    /// from the area the record was opened with (exactly its length), else why not.
    pub fn bytes_of(&self, file: OutFileRef<'b>) -> Result<&'b [u8], StoredError> {
        match file {
            OutFileRef::Inline(bytes) => Ok(bytes),
            OutFileRef::Stored(stored) => {
                let area = self.area.ok_or(StoredError { sha256: *stored.sha256, why: StoredWhy::NoArea })?;
                let bytes = area.bytes(&stored)?;
                if bytes.len() as u64 != stored.len {
                    return Err(StoredError { sha256: *stored.sha256, why: StoredWhy::Length { found: bytes.len() as u64 } });
                }
                Ok(bytes)
            }
        }
    }

    /// The stored files, sorted by name.
    pub fn stored(&self) -> impl Iterator<Item = StoredRef<'b>> + use<'b> {
        let (at, n) = self.stored_at;
        let mut cur = Cursor { bytes: self.bytes, at };
        (0..n).map_while(move |_| stored_entry(&mut cur).ok())
    }

    /// Every file, inline then stored (each section sorted by name; no name in both).
    pub fn all_files(&self) -> impl Iterator<Item = (&'b str, OutFileRef<'b>)> + use<'b> {
        let inline = self.files().map(|(name, bytes)| (name, OutFileRef::Inline(bytes)));
        inline.chain(self.stored().map(|stored| (stored.name, OutFileRef::Stored(stored))))
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

    /// The inline files, (name, bytes), sorted by name; the stored ones are [`Self::stored`].
    pub fn files(&self) -> impl Iterator<Item = (&'b str, &'b [u8])> + use<'b> {
        self.entries(10).filter_map(|e| if let Entry::File(n, b) = e { Some((n, b)) } else { None })
    }

    /// The file `name`, inline or stored, by a scan of the sorted names.
    pub fn file(&self, name: &str) -> Option<OutFileRef<'b>> {
        self.all_files().find(|(n, _)| *n == name).map(|(_, file)| file)
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
        for stored in self.stored() {
            out.stored.push(StoredFile { name: own(stored.name, alloc), len: stored.len, mode: stored.mode, sha256: *stored.sha256 });
        }
        out
    }
}
