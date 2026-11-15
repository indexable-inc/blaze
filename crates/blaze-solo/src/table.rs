//! The leaf table: one a checkout, local to the machine (docs/v2/plan/design/end-state/
//! daemon.md 4.2). Sorted rows `(path, stamp, content id or not hashed yet, kind)` for every
//! file and directory of the view, and the instant they were looked at.
//!
//! The layout, little-endian, the one home of it:
//!
//! ```text
//! magic "BZLT" | layout u32 | instant_ns u64 | count u32 | count x row | crc32c u32
//! row: kind u8 | hashed u8 (0 or 1) | path_len u32 | path | stamp (36 B) | id (32 B, if hashed)
//! ```
//!
//! Rows are sorted by path, bytewise, each once; the checkout's own directory is the row of
//! the empty path, first. The check covers every byte before it. A table of another layout
//! word is refused by that word ([`Undecoded::Layout`]), a torn or rotten one by its check:
//! never read as an empty table.
//!
//! `instant_ns` is the wall clock read before the first stat of the run that wrote it, a lower
//! bound of every look the rows record: the racy rule asks whether a stamp is older than it by
//! more than the window, and an instant earlier than the true looks only makes more stamps
//! racy, never fewer.

#![blaze::sans_io]

use std::fmt;

use blaze_core::Stamp;
use blaze_ids::{ContentId, crc32c::crc32c};

pub const MAGIC: [u8; 4] = *b"BZLT";
/// The layout word this crate writes and reads; a table of any other is refused by it.
pub const LAYOUT: u32 = 1;
const HEAD: usize = 4 + 4 + 8 + 4;
const CHECK: usize = 4;

/// What a row is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Kind {
    /// A file in the view: its bytes may be read by a step.
    File = 0,
    /// A directory in the view.
    Dir = 1,
    /// A symbolic link, never followed.
    Link = 2,
    /// A directory out of the view by its `CACHEDIR.TAG`: nothing under it is a row but the tag.
    Hidden = 3,
    /// The `CACHEDIR.TAG` that hides its directory: stamped each run, probed when it moved.
    Tag = 4,
}

impl Kind {
    /// The kind a look at a name finds.
    pub fn of(stamp: &Stamp) -> Kind {
        if stamp.is_dir() {
            Kind::Dir
        } else if stamp.is_link() {
            Kind::Link
        } else {
            Kind::File
        }
    }

    fn from_byte(byte: u8) -> Option<Kind> {
        Some(match byte {
            0 => Kind::File,
            1 => Kind::Dir,
            2 => Kind::Link,
            3 => Kind::Hidden,
            4 => Kind::Tag,
            _ => return None,
        })
    }

    /// Whether a step's read of the path sees something there (`FILE`, `EXISTS`).
    pub fn is_file_like(self) -> bool {
        matches!(self, Kind::File | Kind::Link)
    }
}

/// One row: a path relative to the checkout (`/` between parts, the checkout itself empty).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub path: String,
    pub stamp: Stamp,
    /// The content id of the bytes read under `stamp` (`blaze_vfs::FILE_DOMAIN`'s), or `None`:
    /// not hashed yet, because no step has read the file.
    pub id: Option<ContentId>,
    pub kind: Kind,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Table {
    pub instant_ns: u64,
    pub rows: Vec<Row>,
}

/// Why a table's bytes are not a table, naming the part.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Undecoded {
    Short { len: usize },
    Magic { found: [u8; 4] },
    /// Written by another layout of this crate: refused by its word, never guessed at.
    Layout { found: u32 },
    Check { kept: u32, computed: u32 },
    Row { at: usize, why: &'static str },
    Order { at: usize },
    Trailing { bytes: usize },
}

impl fmt::Display for Undecoded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Undecoded::Short { len } => write!(f, "{len} bytes, fewer than a table's head and check"),
            Undecoded::Magic { found } => write!(f, "magic {found:?} is not a leaf table's {MAGIC:?}"),
            Undecoded::Layout { found } => {
                write!(f, "layout word {found} is not this blaze-solo's layout word {LAYOUT}")
            }
            Undecoded::Check { kept, computed } => {
                write!(f, "its check {kept:#010x} is not its bytes' crc32c {computed:#010x}: torn or rotten")
            }
            Undecoded::Row { at, why } => write!(f, "row {at}: {why}"),
            Undecoded::Order { at } => write!(f, "row {at} is not after the row before it: rows are sorted, each once"),
            Undecoded::Trailing { bytes } => write!(f, "{bytes} bytes after the last row"),
        }
    }
}

impl Table {
    /// The table's bytes, whole.
    pub fn encode(&self) -> Vec<u8> {
        encode(self.instant_ns, &self.rows)
    }

    pub fn decode(bytes: &[u8]) -> Result<Table, Undecoded> {
        if bytes.len() < HEAD + CHECK {
            return Err(Undecoded::Short { len: bytes.len() });
        }
        let magic: [u8; 4] = bytes[0..4].try_into().expect("4 bytes");
        if magic != MAGIC {
            return Err(Undecoded::Magic { found: magic });
        }
        let layout = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
        if layout != LAYOUT {
            return Err(Undecoded::Layout { found: layout });
        }
        let body = bytes.len() - CHECK;
        let kept = u32::from_le_bytes(bytes[body..].try_into().expect("4 bytes"));
        let computed = crc32c(&bytes[..body]);
        if kept != computed {
            return Err(Undecoded::Check { kept, computed });
        }
        let instant_ns = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
        let count = u32::from_le_bytes(bytes[16..20].try_into().expect("4 bytes")) as usize;
        let mut at = HEAD;
        // alloc: the table's rows, once a command; at most one a byte left, so a count the
        // bytes cannot hold reserves no more than they could.
        let mut rows: Vec<Row> = Vec::with_capacity(count.min(body / (2 + 4 + Stamp::BYTES)));
        let body_bytes = &bytes[..body];
        for n in 0..count {
            let take = |at: &mut usize, len: usize| next(body_bytes, at, len, n);
            let head = take(&mut at, 2)?;
            let kind = Kind::from_byte(head[0]).ok_or(Undecoded::Row { at: n, why: "a kind byte no kind has" })?;
            let hashed = match head[1] {
                0 => false,
                1 => true,
                _ => return Err(Undecoded::Row { at: n, why: "a hashed byte that is neither 0 nor 1" }),
            };
            let len = u32::from_le_bytes(take(&mut at, 4)?.try_into().expect("4 bytes")) as usize;
            let path = core::str::from_utf8(take(&mut at, len)?)
                .map_err(|_| Undecoded::Row { at: n, why: "a path that is not UTF-8" })?;
            if !(path.is_empty() && n == 0) && !relative(path) {
                return Err(Undecoded::Row { at: n, why: "a path that is not a relative name under the checkout" });
            }
            let stamp = Stamp::decode(take(&mut at, Stamp::BYTES)?).expect("36 bytes");
            let id = match hashed {
                true => Some(ContentId(take(&mut at, 32)?.try_into().expect("32 bytes"))),
                false => None,
            };
            if rows.last().is_some_and(|last| last.path.as_str() >= path) {
                return Err(Undecoded::Order { at: n });
            }
            // alloc: one path a row, kept by the command's rows
            rows.push(Row { path: path.to_owned(), stamp, id, kind });
        }
        if at != body {
            return Err(Undecoded::Trailing { bytes: body - at });
        }
        Ok(Table { instant_ns, rows })
    }
}

/// The bytes of a table of `rows` looked at from `instant_ns` on, without a [`Table`] made.
#[expect(
    blaze::narrowed_len,
    reason = "the table's layout holds its row count and each path's length in u32; a row's path is a file path, far under 4 GiB, and a table past 2^32 rows needs a typed refusal from encode"
)]
pub fn encode(instant_ns: u64, rows: &[Row]) -> Vec<u8> {
    let size = HEAD
        + CHECK
        + rows.iter().map(|row| 2 + 4 + row.path.len() + Stamp::BYTES + if row.id.is_some() { 32 } else { 0 }).sum::<usize>();
    // alloc: the table's bytes, once a write
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&LAYOUT.to_le_bytes());
    out.extend_from_slice(&instant_ns.to_le_bytes());
    out.extend_from_slice(&u32::try_from(rows.len()).expect("fewer than 2^32 rows").to_le_bytes());
    for row in rows {
        out.push(row.kind as u8);
        out.push(u8::from(row.id.is_some()));
        out.extend_from_slice(&u32::try_from(row.path.len()).expect("a path under 4 GiB").to_le_bytes());
        out.extend_from_slice(row.path.as_bytes());
        row.stamp.encode(&mut out);
        if let Some(id) = &row.id {
            out.extend_from_slice(&id.0);
        }
    }
    let check = crc32c(&out);
    out.extend_from_slice(&check.to_le_bytes());
    debug_assert_eq!(out.len(), size);
    out
}

/// The `len` bytes of `body` at `at`, `at` moved past them; row `n`'s error where they run past
/// its end.
fn next<'b>(body: &'b [u8], at: &mut usize, len: usize, n: usize) -> Result<&'b [u8], Undecoded> {
    let Some(end) = at.checked_add(len).filter(|end| *end <= body.len()) else {
        return Err(Undecoded::Row { at: n, why: "runs past the table's end" });
    };
    let got = &body[*at..end];
    *at = end;
    Ok(got)
}

/// Whether `path` is a relative name: parts between `/`, none empty, `.` or `..`, no NUL.
pub(crate) fn relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}
