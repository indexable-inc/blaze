//! `ar` archives: the static libraries build scripts leave (`libzstd.a`, `librocksdb.a`, ...).
//!
//! The format (FreeBSD ar(5); the GNU binutils `ar` documentation; lld's `Archive` reader in
//! LLVM's `Object/Archive.cpp` as a reference for which forms exist):
//!
//! - the file starts with the global header `!<arch>\n`; `!<thin>\n` is a thin archive whose
//!   members live in other files: refused ([`ArError::Thin`]), since a record must hold its bytes;
//! - each member is a 60-byte header (`ar_name[16]`, `ar_date[12]`, `ar_uid[6]`, `ar_gid[6]`,
//!   `ar_mode[8]`, `ar_size[10]`, `ar_fmag[2]` = "`\n"), all decimal ASCII padded with spaces,
//!   then `ar_size` bytes, then one pad byte when `ar_size` is odd ("Each archive file member
//!   begins on an even byte boundary; a newline is inserted between files if necessary");
//! - BSD names: a name of 16 bytes or less stands in `ar_name` padded with spaces; a longer one
//!   is written `#1/<len>` and its first `<len>` bytes of the member's data are the name (padded
//!   with NULs), counted in `ar_size`;
//! - GNU names: a short name ends with `/`; the member named `//` holds the long names, each
//!   ended by `/\n`, and a member named `/<decimal>` takes its name at that offset of the table;
//! - symbol indexes (`/`, `/SYM64/` in GNU; `__.SYMDEF`, `__.SYMDEF SORTED`, `__.SYMDEF_64`,
//!   `__.SYMDEF_64 SORTED` in BSD) are skipped and never trusted: resolution indexes the members'
//!   own symbol tables (piece W2).
//!
//! Members come out in archive order, which is the order resolution walks them.

use blaze_own::{AVec, Alloc, Vec};

/// The most members one archive may hold (the largest tempo links, rocksdb's, has a few
/// hundred); more is refused [`ArError::TooMany`], never truncated.
pub const MOST_MEMBERS: u32 = 1 << 16;

/// An archive's object members, in archive order (symbol indexes and the long-name table are
/// not members).
pub struct Archive<'a> {
    pub members: AVec<'a, Member<'a>>,
}

/// One member: its name (BSD's NUL padding and GNU's ending `/` taken off), the offset of its
/// header in the archive (the member's identity within it), and its bytes (a BSD long name
/// taken off the front).
pub struct Member<'a> {
    pub name: &'a [u8],
    pub offset: u32,
    pub bytes: &'a [u8],
}

/// Why an archive was refused. `at` is the offset in the archive of the member header the
/// refusal is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArError {
    /// The bytes do not start with `!<arch>\n`.
    NotAnArchive,
    /// The bytes start with `!<thin>\n`: the members are other files, which a record cannot hold.
    Thin,
    /// A header or a member's data runs past the end of the bytes.
    Truncated { at: u32 },
    /// A header breaks the format; `why` names the field.
    Header { at: u32, why: &'static str },
    /// A GNU `/<decimal>` name with no `//` table before it, or an offset outside it.
    LongName { at: u32 },
    /// More than [`MOST_MEMBERS`] members.
    TooMany { most: u32 },
    /// The caller's allocator refused.
    Alloc,
}

const GLOBAL_HEADER: &[u8] = b"!<arch>\n";
const THIN_HEADER: &[u8] = b"!<thin>\n";
const HEADER_LEN: usize = 60;
const FMAG: &[u8] = b"`\n";

/// The symbol-index members' names: skipped, never read.
const SYMBOL_INDEXES: [&[u8]; 6] = [b"/", b"/SYM64/", b"__.SYMDEF", b"__.SYMDEF SORTED", b"__.SYMDEF_64", b"__.SYMDEF_64 SORTED"];

/// Reads an archive's members in place: each name and member's bytes borrow `bytes`.
pub fn read<'a>(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<Archive<'a>, ArError> {
    if bytes.starts_with(THIN_HEADER) {
        return Err(ArError::Thin);
    }
    if !bytes.starts_with(GLOBAL_HEADER) {
        return Err(ArError::NotAnArchive);
    }
    if u32::try_from(bytes.len()).is_err() {
        return Err(ArError::Header { at: 0, why: "an archive of 4 GiB or more (member offsets are 32-bit)" });
    }
    let mut members: AVec<'a, Member<'a>> = Vec::new_in(alloc);
    let mut long_names: Option<&'a [u8]> = None;
    let mut at = GLOBAL_HEADER.len();
    while at < bytes.len() {
        let at32 = offset32(at);
        let header = bytes.get(at..at + HEADER_LEN).ok_or(ArError::Truncated { at: at32 })?;
        if &header[58..60] != FMAG {
            return Err(ArError::Header { at: at32, why: "ar_fmag is not \"`\\n\"" });
        }
        let size = decimal(&header[48..58]).ok_or(ArError::Header { at: at32, why: "ar_size is not a decimal number" })?;
        let data_at = at + HEADER_LEN;
        let data_end = data_at.checked_add(size).ok_or(ArError::Truncated { at: at32 })?;
        let data: &'a [u8] = bytes.get(data_at..data_end).ok_or(ArError::Truncated { at: at32 })?;
        let raw = trim_end(&header[0..16], b' ');

        // `at` moves on before the member is looked at, so every branch below may `continue`.
        at = data_end + (size & 1);

        if SYMBOL_INDEXES.contains(&raw) {
            continue;
        }
        if raw == b"//" {
            if long_names.is_some() {
                return Err(ArError::Header { at: at32, why: "a second GNU long-name table (`//`)" });
            }
            long_names = Some(data);
            continue;
        }
        let (name, body) = if let Some(len) = raw.strip_prefix(b"#1/") {
            // BSD long name: the data's first `len` bytes, NUL-padded.
            let len = decimal(len).ok_or(ArError::Header { at: at32, why: "a BSD `#1/` name length is not a decimal number" })?;
            if len > data.len() {
                return Err(ArError::Header { at: at32, why: "a BSD `#1/` name is longer than its member" });
            }
            (trim_end(&data[..len], 0), &data[len..])
        } else if let Some(offset) = raw.strip_prefix(b"/") {
            // GNU long name: `/<offset>` into the `//` table, ended by `\n` (after a `/`).
            let offset = decimal(offset).ok_or(ArError::Header { at: at32, why: "a GNU `/<offset>` name is not a decimal number" })?;
            let table = long_names.ok_or(ArError::LongName { at: at32 })?;
            let entry = table.get(offset..).ok_or(ArError::LongName { at: at32 })?;
            let end = entry.iter().position(|&b| b == b'\n').ok_or(ArError::LongName { at: at32 })?;
            (strip_slash(&entry[..end]), data)
        } else {
            // A short name: BSD's as written, GNU's with its ending `/`.
            (strip_slash(raw), data)
        };
        if SYMBOL_INDEXES.contains(&name) {
            continue;
        }
        if members.len() >= MOST_MEMBERS as usize {
            return Err(ArError::TooMany { most: MOST_MEMBERS });
        }
        members.try_push(Member { name, offset: at32, bytes: body }).map_err(|_| ArError::Alloc)?;
    }
    Ok(Archive { members })
}

/// `at` as a 32-bit offset: the archive was checked to be under 4 GiB, so every offset in it
/// fits; `u32::MAX` would stand only for a position past that, which `read` never reaches.
fn offset32(at: usize) -> u32 {
    u32::try_from(at).unwrap_or(u32::MAX)
}

/// A decimal ASCII field padded with spaces at its end; `None` when empty, not decimal, or
/// past `usize`.
fn decimal(field: &[u8]) -> Option<usize> {
    let digits = trim_end(field, b' ');
    if digits.is_empty() {
        return None;
    }
    let mut value: usize = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
    }
    Some(value)
}

fn trim_end(bytes: &[u8], pad: u8) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == pad {
        end -= 1;
    }
    &bytes[..end]
}

/// GNU's names end with one `/`; BSD's do not, and a name never ends with one otherwise.
fn strip_slash(name: &[u8]) -> &[u8] {
    name.strip_suffix(b"/").unwrap_or(name)
}
