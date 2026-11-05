//! A `.crate` unpacked in memory, each file's bytes [`Bytes::kept`] (they live as long as the VFS
//! tree that holds them, past the fetch), with cargo's checks (`sources/registry/mod.rs`'s `unpack`):
//! every entry under `<name>-<version>/`, files and directories only, no more bytes than cargo
//! lets one unpack to. Each entry's name is held to blaze-fetch's one set of name rules
//! ([`blaze_fetch::unpack::Names`], which the disk unpacker holds them to too): a path that is
//! not UTF-8, absolute, has a `..`, a `\` or a NUL, a file twice, or an entry under a file is
//! refused, naming the entry.

#![forbid(unsafe_code)]

use std::io::Read as _;

use blaze_fetch::unpack::{NameRules, Names, Twice, is_dir_entry};
use blaze_mem::Tag;
use blaze_registry::download::{LimitErrorReader, is_windows_reserved_path};
use blaze_vfs::{Bytes, Mode};
use tar::{Archive, EntryType};

use super::PACKAGE_SOURCE_LOCK;
use crate::serve::work::SourceFile;

const MAX_UNPACK_SIZE: u64 = 512 * 1024 * 1024;
const MAX_COMPRESSION_RATIO: u64 = 20;

/// The files of `tarball`, a gzipped tar whose entries are under `prefix/`, by their path under
/// it; each error is cargo's cause of `failed to unpack package`.
pub(super) fn unpack(tarball: &[u8], prefix: &str) -> Result<Vec<SourceFile>, String> {
    let limit = MAX_UNPACK_SIZE.max((tarball.len() as u64).saturating_mul(MAX_COMPRESSION_RATIO));
    let gz = flate2::read::GzDecoder::new(tarball);
    let mut tar = Archive::new(LimitErrorReader { inner: gz, left: limit });
    let entries = tar.entries().map_err(|e| format!("failed to iterate over archive: {e}"))?;
    let mut files = Vec::new(); // alloc: the package's files, once a fetch
    let mut names = Names::new(NameRules {
        top: Some(prefix),
        fold: false,
        twice: Twice::Refuse,
        reserved: &[],
        most_path: usize::MAX,
    });
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("failed to iterate over archive: {e}"))?;
        let raw = entry.path_bytes().into_owned(); // alloc: an entry's name, once an entry
        let shown = String::from_utf8_lossy(&raw).into_owned(); // alloc: as above
        let kind = entry.header().entry_type();
        let is_dir = is_dir_entry(entry.header(), &raw);
        if !is_dir && kind != EntryType::Regular {
            return Err(format!(
                "invalid tarball downloaded, contains an entry at {shown:?} with invalid type {kind:?}"
            ));
        }
        let refused =
            |why: blaze_fetch::unpack::UnpackRefused| format!("invalid tarball downloaded: {why}");
        let Some(inside) = names.under_top(&raw).map_err(refused)? else {
            if is_dir {
                continue;
            }
            return Err(format!(
                "invalid tarball downloaded, contains a file at {shown:?} which is the package's directory"
            ));
        };
        if inside.rsplit('/').next() == Some(PACKAGE_SOURCE_LOCK) {
            continue;
        }
        if cfg!(windows) && is_windows_reserved_path(std::path::Path::new(&inside)) {
            return Err(format!(
                "`{shown}` appears to contain a reserved Windows path, it cannot be extracted on Windows"
            ));
        }
        names.claim(&inside, is_dir).map_err(refused)?;
        if is_dir {
            continue;
        }
        let bits = entry
            .header()
            .mode()
            .map_err(|e| format!("failed to unpack entry at `{shown}`: {e}"))?;
        // The entry's bytes in a block of their own, kept past the fetch with the VFS tree
        // that holds them, read straight in: as many as its header says, which the archive's
        // limit bounds (a header claiming more stops here, as the reader would at the limit).
        let size = entry.size();
        let size = usize::try_from(size).ok().filter(|_| size <= limit).ok_or_else(|| {
            format!("failed to unpack entry at `{shown}`: maximum limit reached when reading")
        })?;
        let bytes = Bytes::kept(Tag::Vfs, size, |place| entry.read_exact(place))
            .map_err(|e| format!("failed to unpack entry at `{shown}`: {e}"))?;
        let mode = if bits & 0o111 != 0 { Mode::Executable } else { Mode::Regular };
        files.push(SourceFile { path: inside, bytes, mode });
    }
    Ok(files)
}
