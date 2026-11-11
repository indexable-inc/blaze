//! A directory's listing with every entry's stamp from `getattrlistbulk`, macOS's one call for
//! names and attributes together ([`crate::Effect::Entries`]; docs/macos.md, "File operations").
//!
//! The listing was `readdir` and one `fstatat` per entry: a system call per name. One
//! `getattrlistbulk` fills a buffer with as many entries as fit, each its name and the
//! attributes asked for, so a directory of a few hundred names is listed and stamped in one
//! or two calls. The stamp it makes is the `fstatat`'s, field for field: the file ID is
//! `st_ino`, the data length `st_size` (a directory's own, `ATTR_DIR_DATALENGTH`), the times
//! `st_mtimespec` and `st_ctimespec`, and the access mask with the object's type `st_mode`
//! (the test holds it to that on a directory of files, directories, links and a FIFO).
//!
//! There is no second way to list (rule no-fallback): an entry the file system returns
//! without an attribute asked for fails the listing, naming the entry and the attribute. An
//! entry the system reports an error of (it went between the listing and its attributes, or
//! may not be looked at) is stamped as `fstatat`'s error was: gone, or unknown.
//!
//! One difference is known and kept: a name a volume is mounted on is stamped as the
//! directory under the mount, where `fstatat` stamps the root of the volume mounted there.
//! Nothing lists a directory of mount points for its stamps.

use std::{io, path::Path};

use crate::{FileName, Stamp};

/// What is asked of every entry (`ATTR_CMN_RETURNED_ATTRS` and `ATTR_CMN_NAME` the call
/// requires; `ATTR_CMN_ERROR` so one entry's failure is its own).
const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_NAME
    | ATTR_CMN_ERROR
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_CHGTIME
    | libc::ATTR_CMN_ACCESSMASK
    | libc::ATTR_CMN_FILEID;
/// `ATTR_CMN_ERROR` of <sys/attr.h>, which the libc crate does not name.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;

/// The buffer one call fills: some five hundred entries of a source directory's names.
const BUFFER: usize = 64 << 10;

/// `enum vtype` of <sys/vnode.h>, which `ATTR_CMN_OBJTYPE` is, as `st_mode`'s type bits.
fn type_bits(vtype: u32) -> Option<u32> {
    Some(u32::from(match vtype {
        1 => libc::S_IFREG,
        2 => libc::S_IFDIR,
        3 => libc::S_IFBLK,
        4 => libc::S_IFCHR,
        5 => libc::S_IFLNK,
        6 => libc::S_IFSOCK,
        7 => libc::S_IFIFO,
        _ => return None,
    }))
}

/// One entry's bytes, read field by field from `at`: every field is at a multiple of four
/// bytes and may be at no multiple of eight, so each is read from its bytes.
struct Fields<'a> {
    entry: &'a [u8],
    at: usize,
}

impl<'a> Fields<'a> {
    fn u32(&mut self) -> Option<u32> {
        let bytes = self.entry.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_ne_bytes(bytes.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        let bytes = self.entry.get(self.at..self.at + 8)?;
        self.at += 8;
        Some(u64::from_ne_bytes(bytes.try_into().ok()?))
    }

    /// A `struct timespec` (two 64-bit fields), as nanoseconds the way [`super::stamp_of`]
    /// makes them of a `stat`'s.
    fn time_ns(&mut self) -> Option<u64> {
        let seconds = self.u64()?;
        let nanos = self.u64()?;
        Some(seconds.wrapping_mul(1_000_000_000).wrapping_add(nanos))
    }

    /// An `attrreference_t`'s bytes: its offset is from the reference's own first byte, its
    /// length the name's with its NUL.
    fn name(&mut self) -> Option<&'a [u8]> {
        let (entry, from) = (self.entry, self.at);
        let offset = usize::try_from(self.u32()? as i32).ok()?;
        let len = self.u32()? as usize;
        let bytes = entry.get(from + offset..from + offset + len)?;
        let end = bytes.iter().position(|&it| it == 0).unwrap_or(bytes.len());
        Some(&bytes[..end])
    }
}

/// One entry as the call packed it: its name, and its stamp or the attribute it lacks.
fn entry(entry: &[u8]) -> Option<(&[u8], Result<Stamp, &'static str>)> {
    let mut fields = Fields { entry, at: 4 };
    // `attribute_set_t`: the common, volume, directory, file and fork groups returned.
    let common = fields.u32()?;
    let _volume = fields.u32()?;
    let of_dir = fields.u32()?;
    let of_file = fields.u32()?;
    let _fork = fields.u32()?;
    let has = |bit: u32| common & bit != 0;
    // Right after the set, before the name, whatever its bit.
    let error = if has(ATTR_CMN_ERROR) { fields.u32()? } else { 0 };
    if !has(libc::ATTR_CMN_NAME) {
        return None;
    }
    let name = fields.name()?;
    // The rest in the order of their bits, each there only when returned.
    let vtype = if has(libc::ATTR_CMN_OBJTYPE) { Some(fields.u32()?) } else { None };
    let modified = if has(libc::ATTR_CMN_MODTIME) { Some(fields.time_ns()?) } else { None };
    let changed = if has(libc::ATTR_CMN_CHGTIME) { Some(fields.time_ns()?) } else { None };
    let access = if has(libc::ATTR_CMN_ACCESSMASK) { Some(fields.u32()?) } else { None };
    let id = if has(libc::ATTR_CMN_FILEID) { Some(fields.u64()?) } else { None };
    let dir_len =
        if of_dir & libc::ATTR_DIR_DATALENGTH != 0 { Some(fields.u64()?) } else { None };
    let file_len =
        if of_file & libc::ATTR_FILE_DATALENGTH != 0 { Some(fields.u64()?) } else { None };
    if error != 0 {
        let stamp = match error as i32 {
            libc::ENOENT | libc::ENOTDIR => Stamp::GONE,
            _ => Stamp::UNKNOWN,
        };
        return Some((name, Ok(stamp)));
    }
    let stamp: Result<Stamp, &'static str> = (|| {
        let vtype = vtype.ok_or("ATTR_CMN_OBJTYPE")?;
        let kind = type_bits(vtype).ok_or("an ATTR_CMN_OBJTYPE that is a file's type")?;
        let size = match vtype {
            2 => dir_len.ok_or("ATTR_DIR_DATALENGTH")?,
            _ => file_len.ok_or("ATTR_FILE_DATALENGTH")?,
        };
        Ok(Stamp {
            ino: id.ok_or("ATTR_CMN_FILEID")?,
            size,
            mtime_ns: modified.ok_or("ATTR_CMN_MODTIME")?,
            ctime_ns: changed.ok_or("ATTR_CMN_CHGTIME")?,
            mode: kind | (access.ok_or("ATTR_CMN_ACCESSMASK")? & 0o7777),
        })
    })();
    Some((name, stamp))
}

/// What is directly in `dir` (named `name` under the root), each with its stamp, sorted by
/// name; symbolic links are stamped themselves. As the other Unixes' `entries`, in one
/// `getattrlistbulk` per [`BUFFER`] of entries.
pub(crate) fn entries(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Stamp)>> {
    use std::os::{fd::AsFd as _, unix::ffi::OsStrExt as _};
    // alloc: an error path.
    let failed =
        |why: &str| io::Error::other(format!("{}: getattrlistbulk: {why}", dir.display()));
    let fd = super::open_dir(dir)?;
    // alloc: the listing's payload.
    let mut found = Vec::new();
    // alloc: the listing's one buffer, freed with it.
    let mut buffer = vec![0_u8; BUFFER];
    loop {
        let count = crate::raw::attrs_bulk(fd.as_fd(), COMMON, &mut buffer).map_err(|error| {
            // alloc: an error path.
            io::Error::new(error.kind(), format!("{}: getattrlistbulk: {error}", dir.display()))
        })?;
        // No entry is left.
        if count == 0 {
            break;
        }
        let mut rest = &buffer[..];
        for _ in 0..count {
            let len = rest.first_chunk::<4>().map(|it| u32::from_ne_bytes(*it) as usize);
            let packed = len
                .filter(|len| *len >= 4 && len % 4 == 0)
                .and_then(|len| rest.split_at_checked(len));
            let Some((one, after)) = packed else {
                return Err(failed("an entry that is longer than what the call filled"));
            };
            rest = after;
            let Some((leaf, stamp)) = entry(one) else {
                return Err(failed("an entry with no name, or cut short of its attributes"));
            };
            let leaf = std::ffi::OsStr::from_bytes(leaf);
            let stamp = stamp.map_err(|lacks| {
                // alloc: an error path.
                failed(&format!(
                    "the file system returned no {lacks} for {}; blaze has no other way to \
                     list a directory with its stamps on macOS",
                    leaf.display()
                ))
            })?;
            // alloc: the name, the listing's payload.
            let file = super::child_name(name, leaf, || dir.join(leaf))?;
            found.push((file, stamp));
        }
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use super::*;

    // That `getattrlistbulk` resolves at run time is `raw`'s test, beside the call.

    /// The listing by `readdir` and one `lstat` per name: what the bulk call must equal.
    fn by_stat(dir: &Path, name: &FileName) -> Vec<(FileName, Stamp)> {
        let ns = |s: i64, n: i64| (s as u64).wrapping_mul(1_000_000_000).wrapping_add(n as u64);
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let meta = std::fs::symlink_metadata(entry.path()).unwrap();
                let file =
                    FileName::new(&format!("{name}/{}", entry.file_name().to_str().unwrap()))
                        .unwrap();
                let stamp = Stamp {
                    ino: meta.ino(),
                    size: meta.size(),
                    mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
                    ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
                    mode: meta.mode(),
                };
                (file, stamp)
            })
            .collect();
        found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        found
    }

    /// A directory's listing by `getattrlistbulk` is its listing by `readdir` and `lstat`, name for name and stamp for stamp: files of several sizes and modes, an
    /// empty and a filled directory, a link to a file, a dangling link, a FIFO, and more
    /// names than one buffer holds.
    #[test]
    fn a_bulk_listing_is_the_listing_by_readdir_and_lstat() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("blaze-fs-bulk-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let dir = root.join("listed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("empty"), b"").unwrap();
        std::fs::write(dir.join("small.rs"), b"fn f() {}").unwrap();
        std::fs::write(dir.join("large"), vec![7; 300_000]).unwrap();
        std::fs::write(dir.join("sealed"), b"sealed").unwrap();
        std::fs::set_permissions(dir.join("sealed"), std::fs::Permissions::from_mode(0o444))
            .unwrap();
        std::fs::write(dir.join("run.sh"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::create_dir(dir.join("none")).unwrap();
        std::fs::create_dir(dir.join("some")).unwrap();
        for n in 0..5 {
            std::fs::write(dir.join("some").join(format!("f{n}")), b"x").unwrap();
        }
        std::os::unix::fs::symlink("small.rs", dir.join("link")).unwrap();
        std::os::unix::fs::symlink("nowhere/at/all", dir.join("dangling")).unwrap();
        // std's `mkfifo` is unstable, and this module makes no raw calls.
        let fifo = dir.join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        // More than one buffer's worth: some 100 bytes an entry with a name this long.
        for n in 0..1500 {
            let long = format!("a-name-long-enough-to-fill-the-buffer-sooner-{n:05}.rs");
            std::fs::write(dir.join(long), b"").unwrap();
        }
        let name = FileName::new("listed").unwrap();
        let bulk = entries(&dir, &name).unwrap();
        let stat = by_stat(&dir, &name);
        assert_eq!(bulk.len(), stat.len(), "the listings name different numbers of entries");
        for (bulk, stat) in bulk.iter().zip(&stat) {
            assert_eq!(bulk, stat, "getattrlistbulk and lstat disagree on {}", stat.0);
        }
        let of = |leaf: &str| {
            let file = FileName::new(&format!("listed/{leaf}")).unwrap();
            bulk.iter().find(|it| it.0 == file).unwrap().1
        };
        assert!(of("small.rs").is_file() && of("small.rs").size == 9);
        assert!(of("some").is_dir() && of("link").is_link() && of("dangling").is_link());

        // A directory that is not there fails, as the listing by `readdir` did.
        assert!(entries(&root.join("missing"), &name).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
