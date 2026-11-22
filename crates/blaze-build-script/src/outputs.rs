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
//! then the C inputs, four sections, each a u32 count and its entries:
//!   c-states    u32 count, then each entry: name, u8 kind (0 a folder, 1 held, 2 final),
//!               [32 bytes digest, held only]; sorted by name
//!   c-at        u32 state, one per c-request, in order (the count is c-requests')
//!   c-trees     u32 request (a c-request's index; ascending, distinct), cwd (absolute), then
//!               a u32 count of (name, value), sorted by name, distinct
//!   c-bytes     32 bytes digest, u64 offset, u64 length; sorted by digest, distinct; then
//!               the bytes, at each offset from the end of the entries
//! ```
//!
//! A PROCESS TREE's request (`sh configure`, `make`: c_tasks.rs `is_tree`) reads more than its
//! argv: the folder it was spawned in and its environment (`CC`, `CFLAGS`, `MAKEFLAGS`: what
//! tikv-jemalloc-sys hands its `configure`). Both are kept for it ([`CTree`]), as the script's
//! sandbox gave them (guest paths), so the tree runs later as it was asked. A compile or an
//! `ar` keeps neither: its argv is rooted when recorded (c_tasks.rs `rooted`) and it reads no
//! variable the C tasks do not set.
//!
//! THE C INPUTS. A deferred request is a promise to run LATER what the script asked NOW, so it
//! reads `OUT_DIR` as it was at the spawn, never as the script left it: aws-lc-sys writes
//! `apple_core_services_check.c`, defers its compile and removes the folder; cc-rs rewrites one
//! `flag_check.c` between requests; a script may write `gen.c` twice. Each request names a
//! STATE of `OUT_DIR` ([`CRequest::state`]): every folder and file in it at the spawn, a file
//! by the digest of its bytes ([`snap_digest`]). A new state is taken only at a spawn after
//! `OUT_DIR` was edited (the sandbox's `OutDir` counts its edits); one equal to the last is the
//! last. A file whose bytes are the record's own file of that name ([`Snap::Final`]) is not held
//! twice; any other is held once by digest in `c-bytes` ([`Snap::Held`]). The whole state, not
//! just the request's arguments, is kept: a compile's generated headers (`#include` from
//! `OUT_DIR`) are among its files, and which ones it reads is known only after it runs.
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
/// files, `bzbsout6` the C inputs, `bzbsout7` the process trees' folder and environment
/// ([`CTree`]); a `bzbsout6` record is another layout ([`RecordError::Layout`]) and the run key
/// holds this word (`key.rs`), so an old home's records are never found and are made again
/// (rule 26).
pub const LAYOUT: u64 = u64::from_le_bytes(*b"bzbsout7");

/// The domain of [`snap_digest`].
pub const SNAP_DOMAIN: &str = "blaze build-script c input 1";

/// The digest a C input's bytes are held by.
pub fn snap_digest(bytes: &[u8]) -> [u8; 32] {
    blaze_ids::ContentId::of(SNAP_DOMAIN, bytes).0
}

/// One deferred request: its argv joined by NUL, and the `OUT_DIR` state it saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CRequest<'a> {
    pub argv: AString<'a>,
    /// An index of [`Outputs::c_states`].
    pub state: u32,
}

/// What a process-tree request was spawned with beyond its argv (the module's A PROCESS
/// TREE): the guest folder it ran in and its environment, sorted by name, each name once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CTree<'a> {
    /// An index of [`Outputs::c_requests`].
    pub request: u32,
    /// The guest folder it was spawned in, absolute.
    pub cwd: AString<'a>,
    pub env: AVec<'a, (AString<'a>, AString<'a>)>,
}

/// A [`CTree`] read in place ([`OutputsRef::c_trees`]).
#[derive(Clone, Copy, Debug)]
pub struct CTreeRef<'b> {
    pub request: u32,
    pub cwd: &'b str,
    bytes: &'b [u8],
    env_at: usize,
    env_n: u32,
}

impl<'b> CTreeRef<'b> {
    /// Its environment, sorted by name.
    pub fn env(&self) -> impl Iterator<Item = (&'b str, &'b str)> + use<'b> {
        let mut cur = Cursor { bytes: self.bytes, at: self.env_at };
        (0..self.env_n).map_while(move |_| Some((cur.text().ok()?, cur.text().ok()?)))
    }

    /// The value of `name` in its environment.
    pub fn var(&self, name: &str) -> Option<&'b str> {
        self.env().find(|(n, _)| *n == name).map(|(_, v)| v)
    }
}

/// One entry of an `OUT_DIR` state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Snap {
    Folder,
    /// A file whose bytes are held in the C inputs by this digest ([`snap_digest`]).
    Held([u8; 32]),
    /// A file whose bytes are the record's own inline file of the same name.
    Final,
}

impl Snap {
    fn kind(self) -> u8 {
        match self {
            Snap::Folder => 0,
            Snap::Held(_) => 1,
            Snap::Final => 2,
        }
    }
}

/// `OUT_DIR` at one deferred spawn: its entries by name from it, sorted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CState<'a> {
    pub entries: AVec<'a, (AString<'a>, Snap)>,
}

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
    /// `configure`: `probe::is_deferred_tool`), in spawn order, each with the `OUT_DIR` state
    /// it saw.
    pub c_requests: AVec<'a, CRequest<'a>>,
    /// Sorted by name; no name of `files` among them.
    pub stored: AVec<'a, StoredFile<'a>>,
    /// The `OUT_DIR` states the requests saw (the module's THE C INPUTS).
    pub c_states: AVec<'a, CState<'a>>,
    /// The bytes of each [`Snap::Held`] file, by digest: sorted, distinct
    /// ([`Outputs::hold_c_bytes`]).
    pub c_bytes: AVec<'a, ([u8; 32], AVec<'a, u8>)>,
    /// Each process-tree request's folder and environment, by ascending request.
    pub c_trees: AVec<'a, CTree<'a>>,
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
    /// 1 GiB and 4096 files (NATIVE.md 3.8).
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
            c_states: Vec::new_in(alloc),
            c_bytes: Vec::new_in(alloc),
            c_trees: Vec::new_in(alloc),
        }
    }

    /// `bytes`, whose digest is `digest` ([`snap_digest`]), held once in [`Outputs::c_bytes`].
    pub fn hold_c_bytes(&mut self, digest: [u8; 32], bytes: &[u8], alloc: Alloc<'a>) {
        if let Err(at) = self.c_bytes.binary_search_by(|(held, _)| held.cmp(&digest)) {
            let mut owned = Vec::with_capacity_in(bytes.len(), alloc);
            owned.extend_from_slice(bytes);
            self.c_bytes.insert(at, (digest, owned));
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
        count(out, self.c_states.len());
        for state in self.c_states.iter() {
            count(out, state.entries.len());
            for (name, snap) in state.entries.iter() {
                text(out, name);
                out.push(snap.kind());
                if let Snap::Held(digest) = snap {
                    out.extend_from_slice(digest);
                }
            }
        }
        count(out, self.c_requests.len());
        for request in self.c_requests.iter() {
            out.extend_from_slice(&request.state.to_le_bytes());
        }
        count(out, self.c_trees.len());
        for tree in self.c_trees.iter() {
            out.extend_from_slice(&tree.request.to_le_bytes());
            text(out, &tree.cwd);
            pairs(out, &tree.env);
        }
        count(out, self.c_bytes.len());
        let mut offset = 0u64;
        for (digest, bytes) in self.c_bytes.iter() {
            out.extend_from_slice(digest);
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            offset += bytes.len() as u64;
        }
        for (_, bytes) in self.c_bytes.iter() {
            out.extend_from_slice(bytes);
        }
    }

    /// The layout word `layout`, the `OUT_DIR` and the ten sections before the files.
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
        self.c_requests.iter().for_each(|it| text(out, &it.argv));
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
    /// The C inputs at `at` are not whole: a request's state that is not there, a count of
    /// states other than the requests', a held digest with no bytes, bytes out of order.
    CInput { at: usize },
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
    /// The C inputs' sections: the states, the requests' states, the bytes' entries; each its
    /// start (after its count) and count. `c_blobs` is where the held bytes start.
    c_states: (usize, u32),
    c_at: (usize, u32),
    /// The process trees' section ([`CTree`]): its start (after its count) and count.
    c_trees: (usize, u32),
    c_bytes: (usize, u32),
    c_blobs: usize,
    /// Where its stored files' bytes are read; `None` until [`OutputsRef::with_area`].
    area: Option<&'b dyn StoredBytes>,
}

/// One entry of a C input state, checked.
fn c_entry<'b>(cur: &mut Cursor<'b>) -> Result<(&'b str, Snap), RecordError> {
    let name = cur.text()?;
    let at = cur.at;
    Ok((
        name,
        match cur.u8()? {
            0 => Snap::Folder,
            1 => {
                let mut digest = [0u8; 32];
                digest.copy_from_slice(cur.take(32)?);
                Snap::Held(digest)
            }
            2 => Snap::Final,
            _ => return Err(RecordError::Kind { at }),
        },
    ))
}

/// The width of one `c-bytes` entry: digest, offset, length.
const C_BYTES_ENTRY: usize = 32 + 8 + 8;

/// One `c-trees` entry, read whole (its environment's texts read once, so `cur` is past it).
fn c_tree_entry<'b>(bytes: &'b [u8], cur: &mut Cursor<'b>) -> Result<CTreeRef<'b>, RecordError> {
    let request = cur.u32()?;
    let cwd = cur.text()?;
    let env_n = cur.u32()?;
    let env_at = cur.at;
    for _ in 0..env_n {
        cur.text()?;
        cur.text()?;
    }
    Ok(CTreeRef { request, cwd, bytes, env_at, env_n })
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
        let n = cur.u32()?;
        let c_states = (cur.at, n);
        for _ in 0..n {
            let entries = cur.u32()?;
            for _ in 0..entries {
                c_entry(&mut cur)?;
            }
        }
        let count_at = cur.at;
        let n = cur.u32()?;
        if n != starts[11].1 {
            return Err(RecordError::CInput { at: count_at });
        }
        let c_at = (cur.at, n);
        for _ in 0..n {
            let at = cur.at;
            if cur.u32()? >= c_states.1 {
                return Err(RecordError::CInput { at });
            }
        }
        let n = cur.u32()?;
        let c_trees = (cur.at, n);
        let mut last_request: Option<u32> = None;
        for _ in 0..n {
            let at = cur.at;
            let tree = c_tree_entry(bytes, &mut cur)?;
            if tree.request >= starts[11].1 || last_request.is_some_and(|last| last >= tree.request) || !tree.cwd.starts_with('/') {
                return Err(RecordError::CInput { at });
            }
            last_request = Some(tree.request);
            let mut last_name: Option<&str> = None;
            for (name, _) in tree.env() {
                if last_name.is_some_and(|last| last >= name) {
                    return Err(RecordError::CInput { at });
                }
                last_name = Some(name);
            }
        }
        let n = cur.u32()?;
        let c_bytes = (cur.at, n);
        let mut last: Option<&[u8]> = None;
        let mut offset = 0u64;
        for _ in 0..n {
            let at = cur.at;
            let digest = cur.take(32)?;
            let (from, len) = (cur.u64()?, cur.u64()?);
            if last.is_some_and(|last| last >= digest) || from != offset {
                return Err(RecordError::CInput { at });
            }
            last = Some(digest);
            offset = offset.checked_add(len).ok_or(RecordError::CInput { at })?;
        }
        let c_blobs = cur.at;
        let blobs = usize::try_from(offset).map_err(|_| RecordError::Short { at: c_blobs })?;
        cur.take(blobs)?;
        if cur.at != bytes.len() {
            return Err(RecordError::Trailing { at: cur.at });
        }
        let opened = OutputsRef { bytes, out_dir, starts, stored_at, c_states, c_at, c_trees, c_bytes, c_blobs, area: None };
        // Each held digest has its bytes (a final file is looked up by name where it is laid).
        let mut cur = Cursor { bytes, at: c_states.0 };
        for _ in 0..c_states.1 {
            let entries = cur.u32()?;
            for _ in 0..entries {
                let at = cur.at;
                if let (_, Snap::Held(digest)) = c_entry(&mut cur)?
                    && opened.c_held(&digest).is_none()
                {
                    return Err(RecordError::CInput { at });
                }
            }
        }
        Ok(opened)
    }

    /// Each deferred request's `OUT_DIR` state, in the requests' order ([`Self::c_state`]).
    pub fn c_request_states(&self) -> impl Iterator<Item = u32> + use<'b> {
        let (at, n) = self.c_at;
        let mut cur = Cursor { bytes: self.bytes, at };
        (0..n).map_while(move |_| cur.u32().ok())
    }

    /// Each process-tree request's folder and environment ([`CTree`]), by ascending request.
    pub fn c_trees(&self) -> impl Iterator<Item = CTreeRef<'b>> + use<'b> {
        let (at, n) = self.c_trees;
        let bytes = self.bytes;
        let mut cur = Cursor { bytes, at };
        (0..n).map_while(move |_| c_tree_entry(bytes, &mut cur).ok())
    }

    /// The folder and environment of request `request`, when it is a process tree's.
    pub fn c_tree(&self, request: u32) -> Option<CTreeRef<'b>> {
        self.c_trees().find(|tree| tree.request == request)
    }

    /// How many `OUT_DIR` states the requests saw.
    pub fn c_state_count(&self) -> u32 {
        self.c_states.1
    }

    /// The `OUT_DIR` state `state`: its entries, sorted by name; `None` past the last.
    pub fn c_state(&self, state: u32) -> Option<impl Iterator<Item = (&'b str, Snap)> + use<'b>> {
        if state >= self.c_states.1 {
            return None;
        }
        let mut cur = Cursor { bytes: self.bytes, at: self.c_states.0 };
        for _ in 0..state {
            let entries = cur.u32().ok()?;
            for _ in 0..entries {
                c_entry(&mut cur).ok()?;
            }
        }
        let entries = cur.u32().ok()?;
        Some((0..entries).map_while(move |_| c_entry(&mut cur).ok()))
    }

    /// The held bytes of `digest`, by a search of the sorted entries.
    pub fn c_held(&self, digest: &[u8; 32]) -> Option<&'b [u8]> {
        let (start, n) = self.c_bytes;
        let entry = |at: usize| &self.bytes[start + at * C_BYTES_ENTRY..start + (at + 1) * C_BYTES_ENTRY];
        let (mut lo, mut hi) = (0usize, n as usize);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let one = entry(mid);
            match one[..32].cmp(&digest[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => {
                    let word = |at: usize| u64::from_le_bytes(one[at..at + 8].try_into().unwrap_or([0; 8]));
                    let (from, len) = (usize::try_from(word(32)).ok()?, usize::try_from(word(40)).ok()?);
                    return self.bytes.get(self.c_blobs + from..self.c_blobs + from + len);
                }
            }
        }
        None
    }

    /// The bytes of the state entry `name`, `snap` ([`Self::c_state`]): held ones from the C
    /// inputs, a final one the record's inline file of that name; `None` for a folder or a
    /// final file the record does not hold inline.
    pub fn c_snap_bytes(&self, name: &str, snap: Snap) -> Option<&'b [u8]> {
        match snap {
            Snap::Folder => None,
            Snap::Held(digest) => self.c_held(&digest),
            Snap::Final => match self.file(name)? {
                OutFileRef::Inline(bytes) => Some(bytes),
                OutFileRef::Stored(_) => None,
            },
        }
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
        // The stored files' section runs to the C inputs' first count.
        let (start, n) = self.stored_at;
        hasher.update(&n.to_le_bytes()).part(&self.bytes[start..self.c_states.0 - 4]);
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
        for (argv, state) in self.c_requests().zip(self.c_request_states()) {
            out.c_requests.push(CRequest { argv: own(argv, alloc), state });
        }
        for stored in self.stored() {
            out.stored.push(StoredFile { name: own(stored.name, alloc), len: stored.len, mode: stored.mode, sha256: *stored.sha256 });
        }
        for state in 0..self.c_state_count() {
            let mut entries = Vec::new_in(alloc);
            if let Some(state) = self.c_state(state) {
                state.for_each(|(name, snap)| entries.push((own(name, alloc), snap)));
            }
            out.c_states.push(CState { entries });
        }
        for tree in self.c_trees() {
            let mut env = Vec::new_in(alloc);
            tree.env().for_each(|(a, b)| env.push((own(a, alloc), own(b, alloc))));
            out.c_trees.push(CTree { request: tree.request, cwd: own(tree.cwd, alloc), env });
        }
        let (start, n) = self.c_bytes;
        for at in 0..n as usize {
            let mut digest = [0u8; 32];
            digest.copy_from_slice(&self.bytes[start + at * C_BYTES_ENTRY..start + at * C_BYTES_ENTRY + 32]);
            if let Some(bytes) = self.c_held(&digest) {
                out.hold_c_bytes(digest, bytes, alloc);
            }
        }
        out
    }
}
