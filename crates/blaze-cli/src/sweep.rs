//! The checkout's files (drive.md, "The system side"): every file's path and content id, sorted
//! by path, behind [`Sweeper`] so blaze-solo's parallel stat sweep plugs in where [`ReadAll`]
//! is. [`ReadAll`] reads and hashes every file: bstd's `Meta` holds a length and whether it is a
//! directory, no stamp, so a file cannot be proved unchanged by its stat yet (a gap for bstd's
//! lane; blaze-solo's sweep, sized by `Config::lanes`, needs it too).
//!
//! The leaf table (`.blaze/v2/leaves`) is the last answer's files and ids: what `changed` and
//! `gone` are measured against.

use blaze_drive::{FileEntry, WireError, WireWhy};
use blaze_ids::ContentId;
use blaze_own::{SortedMap, Vec};
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;

/// A file's content id: the same domain as `blaze_vfs::FILE_DOMAIN` (one digest of a file in the
/// tree). blaze-vfs is not a dependency here (std, and much else); the constant belongs in
/// blaze-ids (a gap for its lane, rule 30).
pub const FILE_DOMAIN: &str = "blaze v2 vfs file 1";

/// One file of the checkout as swept.
#[derive(Clone, Copy, Debug)]
pub struct Leaf<'f> {
    /// Relative to the checkout, `/`-separated.
    pub path: &'f str,
    pub id: ContentId,
}

/// What the request's `Start` is made of.
pub trait Sweeper {
    /// Every file under `root` that is part of the checkout, sorted by path, into `out`.
    fn sweep<'f>(&self, cx: &Cx, root: &Dir<ReadWrite>, frame: &'f Frame, out: &mut Vec<Leaf<'f>, &'f Frame>) -> Result<(), CliError>;
}

/// Directories that are never the checkout's: blaze's own, cargo's output, git's.
const NOT_CHECKOUT: [&str; 3] = [".blaze/", "target/", ".git/"];

/// List, read and hash every file.
pub struct ReadAll;

impl Sweeper for ReadAll {
    fn sweep<'f>(&self, cx: &Cx, root: &Dir<ReadWrite>, frame: &'f Frame, out: &mut Vec<Leaf<'f>, &'f Frame>) -> Result<(), CliError> {
        let cx = cx.span("sweep: list, read and hash every file");
        let listed = root.list(&cx, frame)?;
        let mut bytes = Bytes::new_in(frame, "the sweep's file");
        for (path, meta) in listed.iter() {
            let text = path.as_str();
            if meta.dir || NOT_CHECKOUT.iter().any(|skip| text.starts_with(skip)) {
                continue;
            }
            bytes.clear();
            root.read_all(&cx, path, &mut bytes)?;
            let id = ContentId::of(FILE_DOMAIN, bytes.as_slice());
            // The path outlives the listing: copied into the command's frame, freed with it.
            let kept: &'f mut [u8] = Vec::from_slice_in(text.as_bytes(), frame).leak();
            let path = core::str::from_utf8(kept).map_err(|_| CliError::Text { what: "a path bstd listed is not UTF-8" })?;
            out.push(Leaf { path, id });
        }
        out.sort_unstable_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(())
    }
}

/// A manifest: the units' source.
pub fn is_manifest(path: &str) -> bool {
    path == "Cargo.toml" || path == "Cargo.lock" || path.ends_with("/Cargo.toml")
}

const LEAVES: &str = ".blaze/v2/leaves";
const LEAVES_MAGIC: &[u8; 4] = b"BCL1";

/// The last answer's files: path to content id.
pub struct Leaves<'f> {
    rows: SortedMap<&'f [u8], ContentId, &'f Frame>,
}

impl<'f> Leaves<'f> {
    /// The table, or an empty one when there is none (a first build).
    pub fn read(cx: &Cx, root: &Dir<ReadWrite>, frame: &'f Frame) -> Result<Leaves<'f>, CliError> {
        let mut rows = SortedMap::new_in(frame);
        let path = Path::new(LEAVES)?;
        if root.stat(cx, &path)?.is_none() {
            return Ok(Leaves { rows });
        }
        let mut bytes = Bytes::new_in(frame, "the leaf table");
        root.read_all(cx, &path, &mut bytes)?;
        let kept: &'f [u8] = Vec::from_slice_in(bytes.as_slice(), frame).leak();
        let short = |at: usize, why| CliError::Leaves(WireError { what: "leaf table", at: u32::try_from(at).unwrap_or(u32::MAX), why });
        if kept.get(..4) != Some(LEAVES_MAGIC.as_slice()) {
            return Err(short(0, WireWhy::Magic));
        }
        let mut at = 4;
        while at < kept.len() {
            let len = kept.get(at..at + 4).ok_or(short(at, WireWhy::Short))?;
            let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
            let path = kept.get(at + 4..at + 4 + len).ok_or(short(at, WireWhy::Short))?;
            let id = kept.get(at + 4 + len..at + 36 + len).ok_or(short(at, WireWhy::Short))?;
            let mut digest = [0; 32];
            digest.copy_from_slice(id);
            if rows.last_key_value().is_some_and(|(last, _)| *last >= path) {
                return Err(short(at, WireWhy::Unsorted));
            }
            rows.insert(path, ContentId(digest));
            at += 36 + len;
        }
        Ok(Leaves { rows })
    }

    pub fn get(&self, path: &[u8]) -> Option<ContentId> {
        self.rows.get(path).copied()
    }

    /// The paths the table holds that `files` (sorted) does not.
    pub fn gone<'a>(&self, files: &[FileEntry<'a>], out: &mut Vec<&'f [u8], &Frame>) {
        for (path, _) in self.rows.iter() {
            if files.binary_search_by(|file| file.path.cmp(path)).is_err() {
                out.push(*path);
            }
        }
    }

    /// The table of `files`, replacing the old one.
    pub fn write(cx: &Cx, root: &Dir<ReadWrite>, files: &[FileEntry<'_>], frame: &Frame) -> Result<(), CliError> {
        let mut bytes = Vec::new_in(frame);
        bytes.extend_from_slice(LEAVES_MAGIC);
        for file in files {
            let len = u32::try_from(file.path.len()).map_err(|_| CliError::Text { what: "a path past 4 GiB" })?;
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(file.path);
            bytes.extend_from_slice(&file.id.0);
        }
        root.publish(cx, &Path::new(LEAVES)?, Content::bytes(&bytes), Durable::No)?;
        Ok(())
    }
}
