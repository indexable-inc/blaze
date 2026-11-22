//! A verified archive into a record ([`crate::pack`]): pure functions, bytes in and bytes
//! out, each held to [`Limits`] before it writes a byte past them, each writing every file's
//! bytes once, straight into the record.
//!
//! - [`crate_record`]: a `.crate` whose SHA-256 the caller has already compared to the lock's.
//!   It is inflated a piece at a time (blaze-deflate's gzip, bounded in output and ratio) and
//!   each piece read as tar (blaze-tar: checksums, no absolute path or `..`, no device, links
//!   only inside, sizes bounded) into the record, so no whole tar is ever held. Every entry
//!   must lie under `name-version/`, which is stripped. A hard link is refused; a symbolic link
//!   is kept when its target stays inside the folder.
//! - [`git_record`]: the answer to a git `fetch` of one commit: the pack taken out of its
//!   side band and indexed in place (each object's id computed from its bytes, the trailer's
//!   SHA-1 checked, sizes and the base cache bounded), the tree of the pinned commit walked, so
//!   every file is bound to the commit id the lock names, and each blob read straight into its
//!   place in a record sized exactly before a byte is written. A submodule is refused by name.

#![blaze::sans_io]

use blaze_deflate::{GzipDecoder, Limits as Inflate};
use blaze_git_fetch::{
    CacheStats, Found, Index, Kind as GitKind, Mode as GitMode, Oid, PackError, PackLimits, PktWhy, TreeError, TreeLimits,
};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_tar::{Event, Kind as TarKind, Links, Reader};
use blaze_text::{Put, PutDisplay as _, Show, Writes as _};

use crate::pack::{self, Builder, Mode, PackRefused, Sink};
use crate::{Plant, planted};

/// Compressed bytes of a `.crate` inflated at once: the piece they make is held only until the
/// tar reader has taken it (at most this times the ratio limit; about four times this in
/// practice).
pub const PIECE: usize = 16 << 10;

/// Every bound of an unpacking (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The record's files (count, bytes, path length).
    pub pack: pack::Limits,
    /// Bytes of a crate's inflated tar.
    pub most_inflated: u64,
    /// Inflated bytes per compressed byte: past it, a bomb.
    pub most_ratio: u64,
    pub git_pack: PackLimits,
    pub git_tree: TreeLimits,
}

impl Limits {
    /// For real crates and repositories: a crate at most 512 MiB of files (crates.io refuses
    /// uploads past 10 MiB compressed unless raised), 200,000 files, 4 KiB paths; a git tree at
    /// most 2 GiB of objects, read through a base cache of at most 32 MiB.
    pub const REAL: Limits = Limits {
        pack: pack::Limits { most_files: 200_000, most_bytes: 2 << 30, most_path: 4096 },
        most_inflated: 512 << 20,
        most_ratio: 1032,
        git_pack: PackLimits {
            most_objects: 2_000_000,
            most_object: 512 << 20,
            most_total: 2 << 30,
            most_depth: 4096,
            most_cache: 32 << 20,
        },
        git_tree: TreeLimits { most_files: 200_000, most_path: 4096, most_depth: 128 },
    };
}

/// A name cut to 96 bytes, for an error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cut {
    bytes: [u8; 96],
    len: u8,
}

impl Cut {
    pub fn of(raw: &[u8]) -> Cut {
        let len = raw.len().min(96);
        let mut bytes = [0; 96];
        bytes[..len].copy_from_slice(&raw[..len]);
        Cut { bytes, len: len as u8 }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl Show for Cut {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.byte(b'`');
        for chunk in self.bytes().utf8_chunks() {
            out.str(chunk.valid());
            if !chunk.invalid().is_empty() {
                out.str("\u{fffd}");
            }
        }
        out.byte(b'`');
    }
}

/// Why an archive made no record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UnpackError {
    Gzip(blaze_deflate::Error),
    /// Bytes after the gzip member: a `.crate` is exactly one.
    AfterGzip { extra: u64 },
    Tar(blaze_tar::Error),
    /// An entry that is not under the crate's `name-version/` folder.
    Outside { path: Cut },
    HardLink { path: Cut },
    Pack(PackRefused),
    GitAnswer(PktWhy),
    GitPack(PackError),
    GitTree(TreeError),
    /// The pack holds no commit of the pinned id.
    NoCommit,
    Alloc(AllocFailure),
}

impl Show for UnpackError {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            UnpackError::Gzip(e) => out.str("gzip: ").put_display(e),
            UnpackError::AfterGzip { extra } => out.int(*extra).str(" bytes after the gzip member"),
            UnpackError::Tar(e) => out.str("tar: ").put_display(e),
            UnpackError::Outside { path } => out.str("the entry ").show(path).str(" is outside the crate's folder"),
            UnpackError::HardLink { path } => out.str("the entry ").show(path).str(" is a hard link, which a crate does not hold"),
            UnpackError::Pack(why) => out.str("the record: ").show(why),
            UnpackError::GitAnswer(why) => out.str("git's answer: ").str(pkt_why(why)),
            UnpackError::GitPack(e) => out.str("git's pack: ").put_display(e),
            UnpackError::GitTree(e) => out.str("git's tree: ").put_display(e),
            UnpackError::NoCommit => out.str("the pack holds no commit of the pinned id"),
            UnpackError::Alloc(e) => out.put_display(e),
        };
    }
}

/// What is wrong with a pkt-line stream, in words.
pub(crate) fn pkt_why(why: &PktWhy) -> &'static str {
    match why {
        PktWhy::Length => "a line's length is not four hex digits, or runs past the answer",
        PktWhy::Truncated => "it ended inside a line or before its flush",
        PktWhy::Remote => "the server sent an error on side band 3",
        PktWhy::Band { .. } => "a side-band line on no band 1, 2 or 3",
        PktWhy::Unexpected => "a line not the protocol's at that point",
        PktWhy::Alloc(_) => "no memory for its lines",
    }
}

/// What a record holds, and for a git commit how its pack's base cache did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Made {
    pub files: u32,
    pub bytes: u64,
    pub cache: Option<CacheStats>,
}

/// The room a `.crate`'s record takes, known before a byte is inflated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CrateRoom {
    /// The tar's size as the gzip trailer states it, held to the inflate's bounds: a hint, by
    /// which a task chooses a `Put` or a reservation.
    pub stated: u64,
    /// The record's largest possible length, which a reservation is made at: the header, then
    /// at most the inflated bound of data, and of rows and paths (each file's row and path come
    /// from its tar header of 512 bytes or more, or from long-name blocks at least as long),
    /// then the footer. A reservation's unwritten room is a hole, cut off when it is finished.
    pub most: u64,
}

/// The room of the record of `archive`, under `limits`.
pub fn crate_room(archive: &[u8], limits: &Limits) -> CrateRoom {
    let stated = match archive.len().checked_sub(4) {
        Some(at) => u64::from(u32::from_le_bytes(archive[at..].try_into().expect("4 bytes"))),
        None => 0,
    };
    let inflated = limits.most_inflated.min((archive.len() as u64).saturating_mul(limits.most_ratio));
    let most = (pack::HEADER + pack::FOOTER) as u64 + inflated.saturating_mul(2);
    CrateRoom { stated: stated.min(inflated), most }
}

/// The largest record of a git commit under `limits`, which a reservation is made at: its
/// size is known only once the pack is indexed and the tree listed.
pub fn git_room(limits: &Limits) -> u64 {
    let per_file = (pack::ROW + usize::from(limits.pack.most_path)) as u64;
    (pack::HEADER + pack::FOOTER) as u64
        + limits.pack.most_bytes
        + u64::from(limits.pack.most_files).saturating_mul(per_file)
}

/// The record of a `.crate` of `name` at `version` whose SHA-256, `sha256`, the caller checked.
pub fn crate_record<'a>(
    name: &str,
    version: &str,
    sha256: [u8; 32],
    archive: &[u8],
    limits: &Limits,
    alloc: Alloc<'a>,
) -> Result<(Vec<u8, Alloc<'a>>, Made), UnpackError> {
    crate_record_in(name, version, sha256, archive, limits, Vec::new_in(alloc), alloc)
}

/// [`crate_record`], written into `out`: a reservation's bytes ([`pack::Fixed`]) or the heap.
pub fn crate_record_in<'a, S: Sink>(
    name: &str,
    version: &str,
    sha256: [u8; 32],
    archive: &[u8],
    limits: &Limits,
    out: S,
    alloc: Alloc<'a>,
) -> Result<(S, Made), UnpackError> {
    let planted_size = planted(Plant::SizeFromWire);
    let inflate = Inflate { most_out: limits.most_inflated, most_ratio: limits.most_ratio, most_header: 4096 };
    let most_entry = if planted_size { u64::MAX } else { limits.pack.most_bytes };
    let tar_limits = blaze_tar::Limits {
        most_path: u32::from(limits.pack.most_path),
        most_meta: 64 << 10,
        most_entry,
        most_total: most_entry,
        most_entries: limits.pack.most_files.saturating_mul(2).saturating_add(64),
        links: Links::Inside,
    };
    // The tar's size as the gzip trailer states it (ISIZE, mod 2^32), untrusted: only a hint
    // for the record's room, held before anything is reserved to the inflate's own bounds (the
    // limit, and the ratio times the archive's length), so a lie costs no more than a true
    // archive at those bounds could. A trailer that disagrees with the length inflated is
    // refused by the gzip reader (`Why::Size`) before the record is finished, so nothing is
    // stored; a short lie only costs a regrowth.
    let room = if planted_size {
        match archive.len().checked_sub(4) {
            Some(at) => u64::from(u32::from_le_bytes(archive[at..].try_into().expect("4 bytes"))),
            None => 0,
        }
    } else {
        crate_room(archive, limits).stated
    };
    let mut builder =
        Builder::with_sink(pack::Kind::Crate, sha256, limits.pack, room, out, alloc).map_err(UnpackError::Pack)?;
    let mut reader = Reader::new_in(tar_limits, alloc);
    // The folder every entry is under: `name-version/`.
    let mut top: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
    top.try_reserve(name.len() + version.len() + 2).map_err(UnpackError::Alloc)?;
    top.extend_from_slice(name.as_bytes());
    top.push(b'-');
    top.extend_from_slice(version.as_bytes());
    let folder = core::str::from_utf8(&top).expect("two `str`s and a `-`");
    let mut gzip = GzipDecoder::new_in(inflate, alloc);
    let mut piece: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
    let mut fed_at = 0usize;
    // Whether the current entry's bytes are kept (a file) or passed over; whether the tar ended.
    let mut keeping = false;
    let mut ended = false;
    loop {
        piece.clear();
        let input = &archive[fed_at..(fed_at + PIECE).min(archive.len())];
        let fed = gzip.feed(input, &mut piece).map_err(UnpackError::Gzip)?;
        fed_at += fed.consumed;
        let mut at = 0;
        while !ended && at < piece.len() {
            let step = reader.step(&piece[at..]).map_err(UnpackError::Tar)?;
            at += step.consumed;
            match step.event {
                // All of the piece taken: the next one is needed.
                None => break,
                Some(Event::End) => ended = true,
                Some(Event::Data { bytes, .. }) => {
                    if keeping {
                        builder.append(bytes).map_err(UnpackError::Pack)?;
                    }
                }
                Some(Event::Entry(entry)) => {
                    keeping = false;
                    let inside = if entry.path == folder {
                        Some("")
                    } else {
                        entry.path.strip_prefix(folder).and_then(|rest| rest.strip_prefix('/'))
                    };
                    let path = match inside {
                        Some(path) => path,
                        None if planted(Plant::EntryOutside) => entry.path,
                        None => return Err(UnpackError::Outside { path: Cut::of(entry.path.as_bytes()) }),
                    };
                    match entry.kind {
                        TarKind::Dir => {}
                        TarKind::File => {
                            if path.is_empty() {
                                return Err(UnpackError::Outside { path: Cut::of(entry.path.as_bytes()) });
                            }
                            let mode = if entry.mode & 0o111 != 0 { Mode::Exec } else { Mode::File };
                            builder.begin(path, mode, entry.size).map_err(UnpackError::Pack)?;
                            keeping = true;
                        }
                        TarKind::Symlink { target, resolved } => {
                            let stays = resolved == folder || resolved.strip_prefix(folder).is_some_and(|r| r.starts_with('/'));
                            if !stays || path.is_empty() {
                                return Err(UnpackError::Outside { path: Cut::of(entry.path.as_bytes()) });
                            }
                            builder.file(path, Mode::Link, target.as_bytes()).map_err(UnpackError::Pack)?;
                        }
                        TarKind::HardLink { .. } => {
                            return Err(UnpackError::HardLink { path: Cut::of(entry.path.as_bytes()) });
                        }
                    }
                }
            }
        }
        // The member ended, or the archive did (then `finish` says it was cut short).
        if fed.done || input.is_empty() {
            break;
        }
    }
    gzip.finish().map_err(UnpackError::Gzip)?;
    // One member, all of the archive: bytes after it are refused.
    let extra = archive.len() as u64 - gzip.stream_len();
    if extra != 0 {
        return Err(UnpackError::AfterGzip { extra });
    }
    drop(piece);
    reader.finish().map_err(UnpackError::Tar)?;
    let (files, bytes) = builder.counts();
    let record = builder.finish().map_err(UnpackError::Pack)?;
    Ok((record, Made { files, bytes, cache: None }))
}

/// One file of a git commit, listed before the record is sized.
#[derive(Clone, Copy)]
struct Listed {
    path_at: u32,
    path_len: u16,
    mode: Mode,
    object: Found,
}

/// The record of `commit` from `answer`, the body of git's `fetch` answer for it.
pub fn git_record<'a>(
    commit: Oid,
    answer: &[u8],
    most_pack: u64,
    limits: &Limits,
    alloc: Alloc<'a>,
) -> Result<(Vec<u8, Alloc<'a>>, Made), UnpackError> {
    git_record_in(commit, answer, most_pack, limits, Vec::new_in(alloc), alloc)
}

/// [`git_record`], written into `out`: a reservation's bytes ([`pack::Fixed`]) or the heap.
pub fn git_record_in<'a, S: Sink>(
    commit: Oid,
    answer: &[u8],
    most_pack: u64,
    limits: &Limits,
    out: S,
    alloc: Alloc<'a>,
) -> Result<(S, Made), UnpackError> {
    let mut bytes = Vec::new_in(alloc);
    blaze_git_fetch::fetched_pack(answer, most_pack, &mut bytes).map_err(UnpackError::GitAnswer)?;
    let mut index = Index::new_in(&bytes, limits.git_pack, alloc).map_err(UnpackError::GitPack)?;
    // The commit is the lock's id, found only if an object of the pack hashes to it.
    let root = if planted(Plant::CommitFromServer) {
        index.objects().find(|object| object.kind == GitKind::Commit).map(|object| object.oid)
    } else {
        Some(commit)
    };
    let root = match root.and_then(|root| index.find(&root)) {
        Some(found) if found.kind == GitKind::Commit => found.oid,
        _ => return Err(UnpackError::NoCommit),
    };
    // Every file listed first, so the record is sized exactly before a byte is written.
    let mut listed: Vec<Listed, Alloc<'a>> = Vec::new_in(alloc);
    let mut names: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
    let mut total = 0u64;
    let mut failed: Option<AllocFailure> = None;
    blaze_git_fetch::files(&mut index, root, limits.git_tree, alloc, |file| {
        if failed.is_some() {
            return;
        }
        let mode = match file.mode {
            GitMode::File => Mode::File,
            GitMode::Exec => Mode::Exec,
            GitMode::Symlink => Mode::Link,
        };
        let row = Listed { path_at: names.len() as u32, path_len: file.path.len() as u16, mode, object: file.object };
        total = total.saturating_add(file.object.size);
        if let Err(why) = names.try_extend_from_slice(file.path.as_bytes()) {
            failed = Some(why);
        } else if let Err(refused) = listed.try_push(row) {
            failed = Some(refused.why);
        }
    })
    .map_err(UnpackError::GitTree)?;
    if let Some(why) = failed {
        return Err(UnpackError::Alloc(why));
    }
    let mut source = [0u8; 32];
    source[..20].copy_from_slice(&commit.0);
    let mut builder =
        Builder::with_sink(pack::Kind::Git, source, limits.pack, total, out, alloc).map_err(UnpackError::Pack)?;
    for row in listed.iter() {
        let at = row.path_at as usize;
        let path = core::str::from_utf8(&names[at..at + usize::from(row.path_len)]).expect("joined `str`s, cut at their ends");
        let slot = builder.slot(path, row.mode, row.object.size).map_err(UnpackError::Pack)?;
        index.read(row.object, slot).map_err(UnpackError::GitPack)?;
    }
    let cache = index.stats();
    drop(index);
    drop(bytes);
    drop(listed);
    drop(names);
    let (files, data) = builder.counts();
    let record = builder.finish().map_err(UnpackError::Pack)?;
    Ok((record, Made { files, bytes: data, cache: Some(cache) }))
}
