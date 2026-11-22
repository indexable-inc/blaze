//! The roster: which blaze binaries on this machine are live (docs/v2/plan/design/remote-read.md,
//! "End state: what a record's key says about its producer, and a home that stays bounded",
//! section 3 "Liveness", piece 8; D87, D93 (6)).
//!
//! A record is kept while its producer belongs to a live binary. The roster is the fact that
//! says which binaries those are: one file a binary that ran,
//! `<home>/.blaze/v2/roster/<hex16 of H(exe path)>`, holding the binary's path, its linker
//! build id and the day it last ran (days since 1970, from the clock the caller passes in:
//! rule 75). It is beside the packs and never in a key.
//!
//! - [`write_own`]: the running binary's entry, written (unsynced, published whole by rename)
//!   when it is absent, names another binary, or is older than today: one stat a command
//!   otherwise. A lost write costs at most a binary looking unused for a day.
//! - [`live`]: every entry, each with its [`State`]. An entry lives while it ran within
//!   [`WINDOW_DAYS`] AND the file at its path still carries its build id (Mach-O `LC_UUID`, ELF
//!   `NT_GNU_BUILD_ID`, read by bstd's `build_id_in` from the file's first [`EXE_HEAD`] bytes).
//!   `blaze` and `blaze-old` both installed: both live. A rebuild replacing `~/.cargo/bin/blaze`:
//!   the old entry is dead at once ([`State::Replaced`]). A dead entry is LISTED, never deleted
//!   here: the collector (piece 10) decides what goes.
//!
//! Entry form (little-endian): `BZRO`, version 1 (u8), the id's length (u8, 1 to 32), the
//! path's length (u16, 1 to [`PATH_MAX`]), the day (u32), the id, the path (UTF-8, absolute).
//! Another version, a cut or over-long entry, is refused by name ([`Form`]), never misread.
//!
//! Planner's choices followed (WAVE-1600 W1-6): the file name hashes the path alone (the doc's
//! section 3 says H(path, build id); with the path alone a rebuilt binary at one path rewrites
//! one entry instead of leaving one per build). Where it was left: `live` takes the `Fs` too,
//! as a binary's path is outside the home (`Dir`s reach only beneath themselves); the
//! binary's first bytes are read with one `read_at` (a `read_sized` asks the whole file's
//! length, and a binary is far longer than the 64 KiB read); roster entries themselves are
//! read with `read_sized` at the length their listing gave.
#![expect(
    dead_code,
    reason = "wave 2: the DRIVER lane's main.rs call writes the entry and the collector (piece 10) reads `live` and `Row::day`; remove with those calls"
)]

use blaze_own::{Alloc, Texts, Vec};
use bstd::cx::Cx;
use bstd::digest::blake3;
use bstd::fs::{Content, Dir, Durable, EntryKind, Fs, FsError, FsKind, HomeError, HomeWhy, ReadWrite, SizedError};
use bstd::host::{BuildId, Host, build_id_in};
use bstd::io::Missing;
use bstd::mem::Bytes;
use bstd::path::{Path, PathError};

#[cfg(test)]
mod tests;

/// The roster's folder beneath blaze's folder in the home (store.rs `HOME_FOLDER`, `.blaze`),
/// beside the store root (store/plain_place.rs `STORE_ROOT`, `v2/store`; this blaze's store is
/// the folder of its forms in it, `STORE_DIR`: [`crate::store::Home::open_store`]).
pub(crate) const ROSTER: &str = "v2/roster";
/// An entry lives while its binary ran within this many days (remote-read.md section 3).
pub const WINDOW_DAYS: u32 = 30;
/// How much of a binary's file is read for its build id.
pub const EXE_HEAD: usize = 64 << 10;
/// The longest path an entry holds: Linux's `PATH_MAX`, above macOS's 1,024.
pub const PATH_MAX: usize = 4096;
/// The most entries a roster may hold (rule 18: a table has a cap). One a binary that ran in
/// the window and was not removed by the collector: a machine with more is refused by name.
pub const MAX_ENTRIES: usize = 256;

const MAGIC: &[u8; 4] = b"BZRO";
const VERSION: u8 = 1;
/// Magic, version, id length, path length, day.
const HEAD: usize = 4 + 1 + 1 + 2 + 4;
const ENTRY_MAX: usize = HEAD + 32 + PATH_MAX;
/// The suffix of a file bstd's `Dir::publish` is writing beside its name (`.<leaf>.<n>.bstd-new`):
/// another command's entry mid-publish, not an entry yet.
const PUBLISHING: &str = ".bstd-new";

/// A planted bug for the test that proves the oracle sees it (rule 23).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RosterPlant {
    None,
    /// Liveness by path alone: a file at the path counts, whatever build id it carries.
    PathOnly,
}

/// What an entry is now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Ran within the window, and its path holds its build id.
    Live,
    /// Last ran more than [`WINDOW_DAYS`] ago.
    Stale,
    /// No file at its path (removed, or its folder gone).
    Gone,
    /// The file at its path carries another build id: a rebuild replaced it.
    Replaced,
    /// The file at its path carries no build id `build_id_in` reads (not a 64-bit
    /// little-endian Mach-O or ELF, a universal binary, or the id past the bytes read).
    NoId,
}

impl State {
    #[must_use]
    pub fn is_live(self) -> bool {
        self == State::Live
    }
}

/// One entry of the roster as [`live`] found it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    /// The entry's file name: hex of the first 8 bytes of BLAKE3(path).
    pub name: [u8; 16],
    pub build_id: BuildId,
    /// The day it last ran, days since 1970.
    pub day: u32,
    pub state: State,
}

/// Every roster entry, sorted by file name (the listing's order: deterministic), with its
/// path and state.
pub struct Live<'a> {
    paths: Texts<Alloc<'a>>,
    rows: Vec<Row, Alloc<'a>>,
}

impl<'a> Live<'a> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Entry `i`.
    #[must_use]
    pub fn row(&self, i: usize) -> &Row {
        &self.rows[i]
    }

    /// Entry `i`'s binary path.
    #[must_use]
    pub fn path(&self, i: usize) -> &str {
        self.paths.text(i)
    }

    /// The build ids of the live entries, in the roster's order (an id twice when two live
    /// paths hold one binary).
    pub fn live_ids(&self) -> impl Iterator<Item = &BuildId> + '_ {
        self.rows.iter().filter(|row| row.state.is_live()).map(|row| &row.build_id)
    }
}

/// Why an entry's bytes are not an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// Fewer bytes than its head or its lengths say.
    Cut,
    /// Not `BZRO`.
    Magic,
    /// A version this binary does not read.
    Version(u8),
    /// An id length of 0 or past 32.
    IdLength(u8),
    /// A path length of 0 or past [`PATH_MAX`].
    PathLength(u16),
    /// The path is not UTF-8, or not absolute.
    NotAPath,
    /// Bytes after the path.
    Trailing,
}

/// Why the roster could not be written or read. Each names the entry or the binary and the
/// rule: remote-read.md section 3, the roster.
#[derive(Debug)]
pub enum RosterError {
    /// The host does not name the running program's file (bstd `Host::exe`).
    NoExe(Missing),
    /// The running program carries no linker build id.
    NoBuildId,
    /// The running program's path is longer than [`PATH_MAX`] bytes.
    PathTooLong { len: usize },
    /// An entry's bytes are not an entry.
    Form { entry: [u8; 16], why: Form },
    /// A file in the roster's folder that is no entry's name (16 lowercase hex digits).
    NotAnEntry { kind: EntryKind },
    /// The roster holds more than [`MAX_ENTRIES`] entries.
    TooMany { found: usize },
    /// An entry changed between its listing and its read (another command rewrote it).
    Moved { entry: [u8; 16], error: SizedError },
    /// A binary's path could not be made a path beneath the filesystem's root.
    BadPath { entry: [u8; 16], error: PathError },
    /// The filesystem's root could not be reached for a binary's folder.
    Home { entry: [u8; 16], error: HomeError },
    Path(PathError),
    Fs(FsError),
    /// No memory for the listing's rows.
    OutOfMemory,
}

impl core::fmt::Display for RosterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        fn hex(name: &[u8; 16]) -> &str {
            // An entry's name is 16 hex digits (`parse_name`, `entry_name`): always UTF-8.
            core::str::from_utf8(name).unwrap_or("(not hex)")
        }
        f.write_str("the roster (remote-read.md section 3): ")?;
        match self {
            RosterError::NoExe(missing) => write!(f, "this program's own file is not named by the host: {missing}"),
            RosterError::NoBuildId => f.write_str("this program carries no linker build id, so no entry can say which binary ran"),
            RosterError::PathTooLong { len } => write!(f, "this program's path is {len} bytes, past the entry's {PATH_MAX}"),
            RosterError::Form { entry, why } => write!(f, "entry {} is not an entry ({why:?})", hex(entry)),
            RosterError::NotAnEntry { kind } => write!(f, "a {kind:?} in {ROSTER} whose name is no entry's (16 hex digits)"),
            RosterError::TooMany { found } => write!(f, "{found} entries in {ROSTER}, past the cap of {MAX_ENTRIES}"),
            RosterError::Moved { entry, error } => write!(f, "entry {} changed while it was read: {error}", hex(entry)),
            RosterError::BadPath { entry, error } => write!(f, "entry {}'s binary path: {error:?}", hex(entry)),
            RosterError::Home { entry, error } => write!(f, "entry {}'s binary folder: {error}", hex(entry)),
            RosterError::Path(error) => write!(f, "{error:?}"),
            RosterError::Fs(error) => write!(f, "{error}"),
            RosterError::OutOfMemory => f.write_str("no memory for the listing's rows"),
        }
    }
}

impl core::error::Error for RosterError {}

impl From<PathError> for RosterError {
    fn from(error: PathError) -> RosterError {
        RosterError::Path(error)
    }
}

/// The running binary's entry, written when absent, naming another binary, or older than
/// `today`. Published whole (unsynced): a reader sees the old entry or the new.
pub fn write_own(cx: &Cx, home: &Dir<ReadWrite>, host: &Host, today: u32) -> Result<(), RosterError> {
    let exe = host.exe().map_err(RosterError::NoExe)?;
    let id = host.build_id().map_err(|_| RosterError::NoBuildId)?;
    write_entry(cx, home, &exe, &id, today)
}

/// The entry of the binary at `exe` with `id`, run on `today`: [`write_own`]'s body, and the
/// test's way to write another binary's entry.
pub(crate) fn write_entry(cx: &Cx, home: &Dir<ReadWrite>, exe: &str, id: &BuildId, today: u32) -> Result<(), RosterError> {
    let name = entry_name(exe);
    let path = entry_path(&name)?;
    if let Some(meta) = home.stat(cx, &path).map_err(RosterError::Fs)? {
        let mut held = Bytes::new_in(cx.frame(), "the roster's own entry");
        home.read_sized(cx, &path, meta.len, &mut held).map_err(|error| RosterError::Moved { entry: name, error })?;
        // An entry of this binary from today stands. Anything else (an older day, another
        // binary at the path, an entry of another form) is replaced whole by this binary's.
        let stands = match decode(held.as_slice()) {
            Ok(old) => old.path == exe && old.id == *id && old.day >= today,
            Err(_) => false,
        };
        if stands {
            return Ok(());
        }
    }
    let mut bytes = [0u8; ENTRY_MAX];
    let len = encode(exe, id, today, &mut bytes)?;
    home.publish(cx, &path, Content::bytes(&bytes[..len]), Durable::No).map_err(RosterError::Fs)?;
    Ok(())
}

/// Every entry in the roster with its state on `today`, in `alloc`. A missing roster has none.
/// `fs` reaches each binary's file, which is outside the home.
pub fn live<'a>(cx: &Cx, fs: &Fs, home: &Dir<ReadWrite>, today: u32, alloc: Alloc<'a>) -> Result<Live<'a>, RosterError> {
    live_with(cx, fs, home, today, alloc, RosterPlant::None)
}

pub(crate) fn live_with<'a>(
    cx: &Cx,
    fs: &Fs,
    home: &Dir<ReadWrite>,
    today: u32,
    alloc: Alloc<'a>,
    plant: RosterPlant,
) -> Result<Live<'a>, RosterError> {
    let mut out = Live { paths: Texts::new_in(alloc), rows: Vec::new_in(alloc) };
    let folder = Path::new(ROSTER)?;
    let Some(entries) = home.entries(cx, &folder).map_err(RosterError::Fs)? else {
        return Ok(out);
    };
    if entries.len() > MAX_ENTRIES {
        return Err(RosterError::TooMany { found: entries.len() });
    }
    out.rows.try_reserve(entries.len()).map_err(|_| RosterError::OutOfMemory)?;
    let mut held = Bytes::new_in(cx.frame(), "a roster entry");
    let mut head = Bytes::new_in(cx.frame(), "a binary's first 64 KiB");
    for i in 0..entries.len() {
        let file = entries.name(i);
        let kind = entries.kind(i);
        if file.starts_with('.') && file.ends_with(PUBLISHING) {
            // Another command's entry mid-publish (bstd's temporary name): its rename makes it
            // an entry, which a later listing reads.
            continue;
        }
        let Some(name) = parse_name(file) else {
            return Err(RosterError::NotAnEntry { kind });
        };
        if kind != EntryKind::File {
            return Err(RosterError::NotAnEntry { kind });
        }
        held.clear();
        let path = folder.join(&Path::new(file)?);
        home.read_sized(cx, &path, entries.len_of(i), &mut held).map_err(|error| RosterError::Moved { entry: name, error })?;
        let entry = decode(held.as_slice()).map_err(|why| RosterError::Form { entry: name, why })?;
        let state = if today.saturating_sub(entry.day) > WINDOW_DAYS {
            State::Stale
        } else {
            head.clear();
            match read_head(cx, fs, name, entry.path, &mut head)? {
                false => State::Gone,
                true => match (build_id_in(head.as_slice()), plant) {
                    (_, RosterPlant::PathOnly) => State::Live,
                    (Some(found), RosterPlant::None) if found == entry.id => State::Live,
                    (Some(_), RosterPlant::None) => State::Replaced,
                    (None, RosterPlant::None) => State::NoId,
                },
            }
        };
        out.paths.try_push(entry.path).map_err(|_| RosterError::OutOfMemory)?;
        out.rows.push(Row { name, build_id: entry.id, day: entry.day, state });
    }
    Ok(out)
}

/// The first [`EXE_HEAD`] bytes of the file at the absolute `exe`, added to `into`; `false`
/// when no file is there.
fn read_head(cx: &Cx, fs: &Fs, entry: [u8; 16], exe: &str, into: &mut Bytes<'_>) -> Result<bool, RosterError> {
    // `decode` checked the path is absolute; a binary directly in `/` has no folder to open.
    let (folder, leaf) = exe.strip_prefix('/').and_then(|rest| rest.rsplit_once('/')).ok_or(RosterError::Form { entry, why: Form::NotAPath })?;
    let folder = Path::new(folder).map_err(|error| RosterError::BadPath { entry, error })?;
    let leaf = Path::new(leaf).map_err(|error| RosterError::BadPath { entry, error })?;
    // The filesystem's root as a second tree, the binary's folder beneath it (`Fs::home`; the
    // simulation keeps it beneath its one root).
    let dir = match fs.home(cx, "/", &folder) {
        Ok(dir) => dir,
        // A file where a folder of the path should be: no binary there.
        Err(HomeError { why: HomeWhy::Fs(FsError { kind: FsKind::NotADirectory, .. }), .. }) => return Ok(false),
        Err(error) => return Err(RosterError::Home { entry, error }),
    };
    let file = match dir.open_read(cx, &leaf) {
        Ok(file) => file,
        Err(FsError { kind: FsKind::NotFound | FsKind::IsADirectory | FsKind::NotADirectory, .. }) => return Ok(false),
        Err(error) => return Err(RosterError::Fs(error)),
    };
    file.read_at(cx, 0, EXE_HEAD, into).map_err(RosterError::Fs)?;
    Ok(true)
}

/// An entry's file name: hex of BLAKE3(path)'s first 8 bytes.
fn entry_name(exe: &str) -> [u8; 16] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = blake3(exe.as_bytes());
    let mut name = [0u8; 16];
    for (i, byte) in digest[..8].iter().enumerate() {
        name[2 * i] = HEX[usize::from(byte >> 4)];
        name[2 * i + 1] = HEX[usize::from(byte & 0xf)];
    }
    name
}

/// `file` as an entry's name, when it is 16 lowercase hex digits.
fn parse_name(file: &str) -> Option<[u8; 16]> {
    let bytes: [u8; 16] = file.as_bytes().try_into().ok()?;
    bytes.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)).then_some(bytes)
}

fn entry_path(name: &[u8; 16]) -> Result<Path, RosterError> {
    let name = core::str::from_utf8(name).map_err(|_| RosterError::Form { entry: *name, why: Form::NotAPath })?;
    Ok(Path::new(ROSTER)?.join(&Path::new(name)?))
}

/// An entry read in place.
struct Entry<'b> {
    path: &'b str,
    id: BuildId,
    day: u32,
}

fn encode(exe: &str, id: &BuildId, day: u32, out: &mut [u8; ENTRY_MAX]) -> Result<usize, RosterError> {
    let path = exe.as_bytes();
    if path.len() > PATH_MAX {
        return Err(RosterError::PathTooLong { len: path.len() });
    }
    let id = id.as_bytes();
    // A `BuildId` holds 1 to 32 bytes; a path at most `PATH_MAX` (checked above): both fit.
    let id_len = u8::try_from(id.len()).map_err(|_| RosterError::NoBuildId)?;
    let path_len = u16::try_from(path.len()).map_err(|_| RosterError::PathTooLong { len: path.len() })?;
    out[..4].copy_from_slice(MAGIC);
    out[4] = VERSION;
    out[5] = id_len;
    out[6..8].copy_from_slice(&path_len.to_le_bytes());
    out[8..12].copy_from_slice(&day.to_le_bytes());
    let id_end = HEAD + id.len();
    out[HEAD..id_end].copy_from_slice(id);
    let end = id_end + path.len();
    out[id_end..end].copy_from_slice(path);
    Ok(end)
}

fn decode(bytes: &[u8]) -> Result<Entry<'_>, Form> {
    let head = bytes.get(..HEAD).ok_or(Form::Cut)?;
    if &head[..4] != MAGIC {
        return Err(Form::Magic);
    }
    if head[4] != VERSION {
        return Err(Form::Version(head[4]));
    }
    let id_len = head[5];
    if id_len == 0 || id_len > 32 {
        return Err(Form::IdLength(id_len));
    }
    let path_len = u16::from_le_bytes([head[6], head[7]]);
    if path_len == 0 || usize::from(path_len) > PATH_MAX {
        return Err(Form::PathLength(path_len));
    }
    let day = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let id_end = HEAD + usize::from(id_len);
    let end = id_end + usize::from(path_len);
    let id = BuildId::of(bytes.get(HEAD..id_end).ok_or(Form::Cut)?).ok_or(Form::IdLength(id_len))?;
    let path = core::str::from_utf8(bytes.get(id_end..end).ok_or(Form::Cut)?).map_err(|_| Form::NotAPath)?;
    if !path.starts_with('/') {
        return Err(Form::NotAPath);
    }
    if bytes.len() != end {
        return Err(Form::Trailing);
    }
    Ok(Entry { path, id, day })
}
