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

use blaze_own::Share;
use blaze_sys::{Errno, posix::proc};
use crate::{DirName, FileName, Meta, Stamp};

/// What is asked of every entry (`ATTR_CMN_RETURNED_ATTRS` and `ATTR_CMN_NAME` the call
/// requires; `ATTR_CMN_ERROR` so one entry's failure is its own).
const COMMON: u32 = proc::ATTR_CMN_RETURNED_ATTRS
    | proc::ATTR_CMN_NAME
    | ATTR_CMN_ERROR
    | proc::ATTR_CMN_DEVID
    | proc::ATTR_CMN_OBJTYPE
    | proc::ATTR_CMN_MODTIME
    | proc::ATTR_CMN_CHGTIME
    | proc::ATTR_CMN_ACCESSMASK
    | proc::ATTR_CMN_FILEID;
/// `ATTR_CMN_ERROR` of <sys/attr.h>, which the libc crate does not name.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;

/// The buffer one call fills: some five hundred entries of a source directory's names.
const BUFFER: usize = 64 << 10;

/// `enum vtype` of <sys/vnode.h>, which `ATTR_CMN_OBJTYPE` is, as `st_mode`'s type bits.
fn type_bits(vtype: u32) -> Option<u32> {
    Some(u32::from(match vtype {
        1 => proc::S_IFREG,
        2 => proc::S_IFDIR,
        3 => proc::S_IFBLK,
        4 => proc::S_IFCHR,
        5 => proc::S_IFLNK,
        6 => proc::S_IFSOCK,
        7 => proc::S_IFIFO,
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

/// One entry as the call packed it: its name, its stamp or the attribute it lacks, and its
/// device (`ATTR_CMN_DEVID`, the `st_dev` a `stat` gives) where the call returned one.
fn entry(entry: &[u8]) -> Option<(&[u8], Result<Stamp, &'static str>, Option<u64>)> {
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
    if !has(proc::ATTR_CMN_NAME) {
        return None;
    }
    let name = fields.name()?;
    // The rest in the order of their bits, each there only when returned. `dev_t` is an
    // `i32`, widened as std's `MetadataExt::dev` widens `st_dev`.
    let dev = if has(proc::ATTR_CMN_DEVID) { Some(fields.u32()? as i32 as u64) } else { None };
    let vtype = if has(proc::ATTR_CMN_OBJTYPE) { Some(fields.u32()?) } else { None };
    let modified = if has(proc::ATTR_CMN_MODTIME) { Some(fields.time_ns()?) } else { None };
    let changed = if has(proc::ATTR_CMN_CHGTIME) { Some(fields.time_ns()?) } else { None };
    let access = if has(proc::ATTR_CMN_ACCESSMASK) { Some(fields.u32()?) } else { None };
    let id = if has(proc::ATTR_CMN_FILEID) { Some(fields.u64()?) } else { None };
    let dir_len =
        if of_dir & proc::ATTR_DIR_DATALENGTH != 0 { Some(fields.u64()?) } else { None };
    let file_len =
        if of_file & proc::ATTR_FILE_DATALENGTH != 0 { Some(fields.u64()?) } else { None };
    if error != 0 {
        let stamp = if error == u32::from(Errno::NoEnt.raw()) || error == u32::from(Errno::NotDir.raw()) {
            Stamp::GONE
        } else {
            Stamp::UNKNOWN
        };
        return Some((name, Ok(stamp), dev));
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
            dev: dev.ok_or("ATTR_CMN_DEVID")?,
            mode: kind | (access.ok_or("ATTR_CMN_ACCESSMASK")? & 0o7777),
        })
    })();
    Some((name, stamp, dev))
}

/// What is directly in `dir` (named `name` under the root), each with its stamp, sorted by
/// name; symbolic links are stamped themselves. As the other Unixes' `entries`, in one
/// `getattrlistbulk` per [`BUFFER`] of entries.
pub(crate) fn entries(dir: &Path, name: &DirName) -> io::Result<crate::Listing> {
    use std::os::unix::ffi::OsStrExt as _;
    // clone: the listing's directory, a refcount.
    let mut found = crate::ListingBuilder::new(name.share());
    each(dir, |leaf, stamp, _| Ok(found.push(leaf.as_bytes(), stamp)?))?;
    Ok(found.finish()?)
}

/// What is directly in `dir` (named `parent`), each with its [`Meta`] from the same
/// `getattrlistbulk` that listed it (the device included), appended to `out` unsorted: the
/// step of [`super::list`]'s walk. An entry gone between the listing and its attributes is
/// left out; one the system would say nothing of fails the listing, naming it.
pub(crate) fn listed(dir: &Path, parent: &DirName, out: &mut Vec<(FileName, Meta)>) -> io::Result<()> {
    each(dir, |leaf, stamp, dev| {
        if stamp.is_gone() {
            return Ok(());
        }
        let refused = |why: &str| {
            // alloc: an error path.
            io::Error::other(format!("{}: getattrlistbulk: {why}", dir.join(leaf).display()))
        };
        if stamp.is_unknown() {
            return Err(refused("the system would say nothing of it"));
        }
        let dev = dev.ok_or_else(|| refused("no ATTR_CMN_DEVID returned"))?;
        // alloc: the name, the listing's payload.
        out.push((super::name_under(parent, leaf, || dir.join(leaf))?, Meta::of(&stamp, dev)));
        Ok(())
    })
}

/// Each entry of `dir`, in the order the system packs them, handed to `put` with its stamp
/// and device: one `getattrlistbulk` per [`BUFFER`] of entries.
fn each(
    dir: &Path,
    mut put: impl FnMut(&std::ffi::OsStr, Stamp, Option<u64>) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::{fd::AsFd as _, unix::ffi::OsStrExt as _};
    // alloc: an error path.
    let failed =
        |why: &str| io::Error::other(format!("{}: getattrlistbulk: {why}", dir.display()));
    let fd = super::open_dir(dir)?;
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
            let Some((leaf, stamp, dev)) = entry(one) else {
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
            put(leaf, stamp, dev)?;
        }
    }
    Ok(())
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
                    dev: meta.dev(),
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
        let root = crate::temp::TempDir::for_test("blaze-fs-bulk-");
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
        let bulk = entries(&dir, &name.clone().into()).unwrap();
        let stat = by_stat(&dir, &name);
        assert_eq!(bulk.len(), stat.len(), "the listings name different numbers of entries");
        for (at, stat) in stat.iter().enumerate() {
            assert_eq!(
                (bulk.file(at), bulk.stamp(at)),
                stat.clone(),
                "getattrlistbulk and lstat disagree on {}",
                stat.0
            );
        }
        let of = |leaf: &str| bulk.stamp(bulk.find(leaf).unwrap());
        assert!(of("small.rs").is_file() && of("small.rs").size == 9);
        assert!(of("some").is_dir() && of("link").is_link() && of("dangling").is_link());

        // The walk of `Effect::List`, a bulk call per directory, says of each name what an
        // `lstat` says, the device the bulk call returns included, through the nested files.
        let listed = super::super::list(&dir, &DirName::Named(name.clone())).unwrap();
        let mut walked = stat.clone();
        walked.extend(by_stat(&dir.join("some"), &FileName::new("listed/some").unwrap()));
        walked.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(listed.len(), walked.len(), "the walk names different numbers of entries");
        for ((file, meta), (want, stamp)) in listed.iter().zip(&walked) {
            assert_eq!(file, want);
            let lstat = std::fs::symlink_metadata(dir.join(&file.as_str()["listed/".len()..])).unwrap();
            assert_eq!(*meta, Meta::of(stamp, lstat.dev()), "the walk and lstat disagree on {file}");
            assert_ne!(meta.dev, 0, "no device for {file}");
        }

        // A directory that is not there fails, as the listing by `readdir` did.
        assert!(entries(&root.join("missing"), &name.into()).is_err());
    }

    /// The cost of one listing of 1,000 and of 10,000 entries: the floor (one `getattrlistbulk`
    /// pass, nothing kept), the columns ([`entries`]), and the pairs it replaced (a `FileName`
    /// made per entry and the pairs sorted), each the best of 20 rounds, and the allocations of
    /// one. Run by hand, in release: `lanecheck.sh blaze-core test "--release --lib -- listing_cost --ignored --nocapture"`.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn listing_cost() {
        let pairs = |dir: &Path, name: &FileName| -> Vec<(FileName, Stamp)> {
            let mut found = Vec::new();
            each(dir, |leaf, stamp, _| {
                found.push((super::super::child_name(name, leaf, || dir.join(leaf))?, stamp));
                Ok(())
            })
            .unwrap();
            found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            found
        };
        let best = |f: &mut dyn FnMut()| {
            (0..20)
                .map(|_| {
                    let start = std::time::Instant::now();
                    f();
                    start.elapsed()
                })
                .min()
                .unwrap()
        };
        let root = crate::temp::TempDir::for_test("blaze-fs-bulk-cost-");
        for n in [1_000, 10_000] {
            let dir = root.join(format!("d{n}"));
            std::fs::create_dir_all(&dir).unwrap();
            // Created in an order that is not the sorted one.
            for at in 0..n {
                std::fs::write(dir.join(format!("{:05}-file.rs", (at * 7919) % n)), b"").unwrap();
            }
            let name = FileName::new(&format!("d{n}")).unwrap();
            let listed: DirName = name.clone().into();
            // Interleaved, so the three see one state of the caches and the clock.
            let most = std::time::Duration::MAX;
            let (mut floor, mut columns, mut old) = (most, most, most);
            for _ in 0..5 {
                floor = floor.min(best(&mut || each(&dir, |_, _, _| Ok(())).unwrap()));
                columns = columns.min(best(&mut || drop(entries(&dir, &listed).unwrap())));
                old = old.min(best(&mut || drop(pairs(&dir, &name))));
            }
            let ((), floor_used) = blaze_alloc_count::measure(|| each(&dir, |_, _, _| Ok(())).unwrap());
            let (listing, columns_used) = blaze_alloc_count::measure(|| entries(&dir, &listed).unwrap());
            let (old_pairs, old_used) = blaze_alloc_count::measure(|| pairs(&dir, &name));
            assert_eq!(listing.len(), old_pairs.len());
            for (at, (file, stamp)) in old_pairs.iter().enumerate() {
                assert_eq!((&listing.file(at), listing.stamp(at)), (file, *stamp));
            }
            println!(
                "listing of {n}: floor {floor:?} ({} allocations, {} bytes); columns {columns:?} ({} allocations, {} bytes, {} held); pairs {old:?} ({} allocations, {} bytes); counting allocator: {}",
                floor_used.allocations,
                floor_used.bytes,
                columns_used.allocations,
                columns_used.bytes,
                listing.heap_bytes(),
                old_used.allocations,
                old_used.bytes,
                blaze_alloc_count::installed(),
            );
        }
    }
}
