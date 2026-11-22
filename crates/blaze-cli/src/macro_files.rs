//! A workspace member proc-macro crate's files, whole, as one leaf row (piece
//! member-macro-module-2; rules 10, 21 and 65).
//!
//! A member macro crate's module is keyed by `files <sha256>`: `blaze_proc_macro::module::
//! files_digest` over every regular file of its folder the one package rule takes
//! (blaze-build-script `served::in_package`: no VCS entry, no `target` or `.blaze` folder at the
//! crate's root), each by its package-relative path and its bytes. That digest has one home,
//! blaze-proc-macro's, and the bootstrap builder computes the same one for a path package
//! (scripts/src/bin/proc_macro_modules.rs `Origin::Path`).
//!
//! The command reads it here, through the effect layer (the checkout's `Dir` and the command's
//! `Cx`), and keeps it as a [`LeafKind::MacroFiles`] row: the row's id IS the digest. The layout
//! reads one such row per member macro crate (units.rs `lay`), so the digest is in the units'
//! source, hence in the dependency graph's source and in every `MacroModule` part's source: an
//! edit of any file of the crate moves the module lookup's key, and a module found for the old
//! files is never used for the new ones. The `MacroModule` task takes the digest from the row
//! (`blaze_world::procs::identify_files`) and reads no file again.
//!
//! Beside the digest row the layout keeps one [`LeafKind::MacroFile`] row a file (its stamp and
//! id, from the same read as the digest) and one [`LeafKind::MacroFolder`] row a folder (its
//! stamp and the listing of what the digest walks), by the sweep's one stamp mechanism
//! (sweep.rs `look`, `dir_stamp`, `listing_id`; piece member-macro-stamp, rule 77). The sweep
//! follows those as it follows manifests and listings, and the digest row stands while they all
//! do ([`proven`]): an unchanged crate costs one stat a file and a folder and no read; a file
//! touched with the same bytes is read once and the digest stands; an edit, or a file made or
//! removed, moves a row, and the layout reads the crate whole once (rule 21).

use std::alloc::Global;
use std::string::String;

use blaze_build_script::served::{InPackage, in_package};
use blaze_ids::ContentId;
use bstd::cx::Cx;
use bstd::fs::{Dir, EntryKind, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use blaze_own::{Texts, Vec};

use crate::CliError;
use crate::sweep::{ABSENT, Leaf, LeafKind, Listing, Plant, Rows, Stamp, Stamps, dir_stamp, listing_id, look};

/// Why a member macro crate's files could not be read whole: each names the path, relative to
/// the checkout.
#[derive(Debug)]
pub enum MacroFilesError {
    /// An entry whose kind the listing could not tell, or that went between the listing and its
    /// read: the crate changed while it was read, or the file system does not say.
    Kind { at: String },
    /// A folder beneath the crate's that went while the crate was read.
    Gone { at: String },
    /// The digest blaze-proc-macro made is not 64 hex digits (its form changed).
    Digest { dir: String },
}

impl core::fmt::Display for MacroFilesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MacroFilesError::Kind { at } => write!(f, "`{at}`, a file of a member proc-macro crate, is neither a file nor a folder as listed (it changed while read, or the file system does not say): the crate's module key covers every file (macro_files.rs)"),
            MacroFilesError::Gone { at } => write!(f, "the folder `{at}` of a member proc-macro crate went while the crate was read (macro_files.rs)"),
            MacroFilesError::Digest { dir } => write!(f, "the files digest of the member proc-macro crate at `{dir}` is not 64 hex digits (macro_files.rs)"),
        }
    }
}

/// `rel` beneath the member folder `dir`, relative to the checkout.
fn under(dir: &str, rel: &str) -> String {
    match (dir.is_empty(), rel.is_empty()) {
        (true, _) => String::from(rel),
        (false, true) => String::from(dir),
        (false, false) => std::format!("{dir}/{rel}"),
    }
}

/// The entries of the folder `at` (relative to the checkout, "" for its root; `place`: the
/// macro crate's own folder or one beneath it) that the files digest walks, in name order
/// (bstd's `Dir::entries` sorts them): each folder and regular file the one package rule takes
/// (blaze-build-script `served::in_package`, as `blaze_proc_macro::module::package_files`
/// walks). A link and a device are left out, as `package_files` leaves them out; an entry
/// whose kind the listing cannot tell is refused by name. `None` when no folder is there.
pub(crate) fn walked<'f>(cx: &Cx, root: &Dir<ReadWrite>, at: &str, place: InPackage, frame: &'f Frame) -> Result<Option<Listing<'f>>, CliError> {
    let listed = match at.is_empty() {
        true => root.own_entries(cx)?,
        false => match root.entries(cx, &Path::new(at)?)? {
            Some(listed) => listed,
            None => return Ok(None),
        },
    };
    let mut names: Texts<&'f Frame> = Texts::new_in(frame);
    let mut dirs: Vec<bool, &'f Frame> = Vec::new_in(frame);
    for i in 0..listed.len() {
        let name = listed.name(i);
        let is_dir = match listed.kind(i) {
            EntryKind::Dir => true,
            EntryKind::File => false,
            EntryKind::Link | EntryKind::Other => continue,
            EntryKind::Unknown | EntryKind::Gone => return Err(CliError::MacroFiles(MacroFilesError::Kind { at: under(at, name) })),
        };
        if !in_package(name.as_bytes(), is_dir, place) {
            continue;
        }
        names.push(name);
        dirs.push(is_dir);
    }
    Ok(Some(Listing { names, dirs }))
}

/// A [`LeafKind::MacroFolder`] row today: `old`'s id kept, no listing, when the folder's settled
/// stamp equals `old`'s; else listed again ([`walked`], [`listing_id`]; `place`: the crate's own
/// folder, whose row's path is its `MacroFiles` row's, or one beneath it); absent when no folder
/// is there. The stat comes before the listing: an entry made after it moves the stamp the next
/// sweep sees.
#[expect(clippy::too_many_arguments, reason = "the sweep's handles, the row and where it is in its crate")]
pub(crate) fn folder_now(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    plant: Plant,
    dir: &str,
    place: InPackage,
    old: Option<&Leaf>,
    frame: &Frame,
) -> Result<(u8, Option<Stamp>, ContentId), CliError> {
    let Some(stamp) = dir_stamp(cx, root, stamps, plant, dir)? else {
        return Ok((ABSENT, None, ContentId([0; 32])));
    };
    let kept = old.filter(|old| !old.absent()).and_then(|old| old.stamp.map(|was| (was, old.id)));
    if let (Some(now), Some((was, id))) = (stamp, kept)
        && now == was
    {
        return Ok((0, Some(now), id));
    }
    let entries = walked(cx, root, dir, place, frame)?;
    let (flags, id) = listing_id(entries.as_ref().map(Listing::entries));
    Ok((flags, stamp.filter(|_| flags & ABSENT == 0), id))
}

/// Whether `path` is the folder `dir` or beneath it ("" is the checkout's root: everything).
fn beneath(dir: &str, path: &str) -> bool {
    dir.is_empty() || path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// Whether the [`LeafKind::MacroFiles`] row of the crate at `dir` stands today, with no file of
/// it read: its folder's own [`LeafKind::MacroFolder`] row is in the table, and no
/// [`LeafKind::MacroFile`] or [`LeafKind::MacroFolder`] row beneath `dir` changed (`now` against
/// `was`, the sweep's rows today and in the table, index by index). Sound because the layout
/// wrote the digest and those rows from one read of each file ([`read_row`]): equal ids and
/// equal listings are equal bytes under equal paths, so an equal digest. A row beneath `dir`
/// that is another crate's (a nested member) can only make it not stand: the safe side.
pub(crate) fn proven(now: &Rows<'_>, was: &[(u8, ContentId)], dir: &str) -> bool {
    let mut own = false;
    for (i, leaf) in now.rows.iter().enumerate() {
        if !matches!(leaf.kind, LeafKind::MacroFile | LeafKind::MacroFolder) {
            continue;
        }
        let path = now.path(leaf);
        if !beneath(dir, path) {
            continue;
        }
        own |= leaf.kind == LeafKind::MacroFolder && path == dir;
        if leaf.state() != was[i] {
            return false;
        }
    }
    own
}

/// The 32 bytes of a SHA-256 written as 64 hex digits; `None` for any other text.
fn digest_bytes(hex: &str) -> Option<[u8; 32]> {
    let text = hex.as_bytes();
    if text.len() != 64 {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (k, byte) in out.iter_mut().enumerate() {
        *byte = digit(text[2 * k])? << 4 | digit(text[2 * k + 1])?;
    }
    Some(out)
}

/// The row's id as the hex `blaze_proc_macro::module::files_digest` wrote: what a path macro
/// crate's module identity holds (`blaze_world::procs::identify_files`).
pub(crate) fn files_hex(id: &ContentId) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in id.0 {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The rows of the member macro crate at `dir` (relative to the checkout, "" for its root), onto
/// `rows`: one [`LeafKind::MacroFolder`] row a folder the digest walks (its stamp before its
/// listing), one [`LeafKind::MacroFile`] row a file (its stamp before its read, by `look`), and
/// the [`LeafKind::MacroFiles`] row whose id is `blaze_proc_macro::module::files_digest` of the
/// very bytes those ids are of; an absent folder row and an absent digest row when no folder is
/// there. Walked with a work list of folders, by the rules `package_files` walks by.
pub(crate) fn read_row(cx: &Cx, root: &Dir<ReadWrite>, stamps: &impl Stamps, dir: &str, rows: &mut Rows<'_>, frame: &Frame) -> Result<(), CliError> {
    let cx = cx.span("macro files: read a member proc-macro crate's files whole, each with its stamp");
    let mut files: std::vec::Vec<(String, std::vec::Vec<u8, Global>), Global> = std::vec::Vec::new_in(Global);
    let mut folders: std::vec::Vec<String, Global> = std::vec::Vec::new_in(Global);
    let mut bytes = Bytes::new_in(frame, "a member proc-macro crate's file");
    folders.push(String::new());
    while let Some(rel) = folders.pop() {
        let at = under(dir, &rel);
        let stamp = dir_stamp(&cx, root, stamps, Plant::None, &at)?;
        let listed = match stamp {
            Some(_) => walked(&cx, root, &at, if rel.is_empty() { InPackage::Root } else { InPackage::Beneath }, frame)?,
            None => None,
        };
        let Some(listed) = listed else {
            if rel.is_empty() {
                rows.push(LeafKind::MacroFolder, ABSENT, dir, None, ContentId([0; 32]));
                rows.push(LeafKind::MacroFiles, ABSENT, dir, None, ContentId([0; 32]));
                return Ok(());
            }
            return Err(CliError::MacroFiles(MacroFilesError::Gone { at }));
        };
        let (flags, id) = listing_id(Some(listed.entries()));
        rows.push(LeafKind::MacroFolder, flags, &at, stamp.flatten(), id);
        for (name, is_dir) in listed.entries() {
            let entry = if rel.is_empty() { String::from(name) } else { std::format!("{rel}/{name}") };
            if is_dir {
                folders.push(entry);
                continue;
            }
            let path = under(dir, &entry);
            let Some((stamp, id)) = look(&cx, root, stamps, Plant::None, &path, None, &mut bytes)? else {
                return Err(CliError::MacroFiles(MacroFilesError::Kind { at: path }));
            };
            rows.push(LeafKind::MacroFile, 0, &path, stamp, id);
            let mut held = std::vec::Vec::new_in(Global);
            held.extend_from_slice(bytes.as_slice());
            files.push((entry, held));
        }
    }
    let hex = blaze_proc_macro::module::files_digest(&mut files);
    let Some(digest) = digest_bytes(&hex) else {
        return Err(CliError::MacroFiles(MacroFilesError::Digest { dir: String::from(dir) }));
    };
    rows.push(LeafKind::MacroFiles, 0, dir, None, ContentId(digest));
    Ok(())
}

/// The digest of the member at `dir` as this command's rows hold it, in hex; `None` when no
/// present row names it.
pub(crate) fn member_hex(rows: &Rows<'_>, dir: &str) -> Option<String> {
    rows.of(LeafKind::MacroFiles).find(|leaf| !leaf.absent() && rows.path(leaf) == dir).map(|leaf| files_hex(&leaf.id))
}

#[cfg(test)]
mod stamp_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The row's id is the very digest blaze-proc-macro writes: hex out of it and back gives the
    /// same text, so `identify_files` keys by the bootstrap builder's digest.
    /// Prints nothing; passes as `.. macro_files::tests::the_row_id_is_the_files_digest ... ok`.
    #[test]
    fn the_row_id_is_the_files_digest() {
        let mut files: std::vec::Vec<(String, std::vec::Vec<u8, Global>), Global> = std::vec::Vec::new_in(Global);
        let mut lib = std::vec::Vec::new_in(Global);
        lib.extend_from_slice(b"#[proc_macro]\npub fn same(input: TokenStream) -> TokenStream { input }\n");
        files.push((String::from("src/lib.rs"), lib));
        let hex = blaze_proc_macro::module::files_digest(&mut files);
        let id = ContentId(digest_bytes(&hex).expect("64 hex digits"));
        assert_eq!(files_hex(&id), hex);
        assert_eq!(digest_bytes("not hex"), None);
    }
}
