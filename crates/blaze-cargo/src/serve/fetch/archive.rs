//! A `.crate` unpacked in memory with cargo's checks (`sources/registry/mod.rs`'s `unpack`):
//! every entry under `<name>-<version>/`, files and directories only, no more bytes than cargo
//! lets one unpack to; and what the VFS can't hold refused, naming the entry: a path that is not
//! UTF-8, absolute, or has an empty, `.` or `..` part, a file twice, a file where a directory is.

#![forbid(unsafe_code)]

use std::io::Read as _;

use blaze_hash::HashSet;
use blaze_registry::download::{LimitErrorReader, is_windows_reserved_path};
use blaze_vfs::Mode;
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
    let mut files = Vec::new();
    let mut paths = Paths::default();
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("failed to iterate over archive: {e}"))?;
        let raw = entry.path_bytes().into_owned();
        let shown = String::from_utf8_lossy(&raw).into_owned();
        let kind = entry.header().entry_type();
        let inside = inside(&raw, &shown, prefix, kind == EntryType::Directory)?;
        let is_dir = match kind {
            EntryType::Regular => false,
            EntryType::Directory => true,
            kind => {
                return Err(format!(
                    "invalid tarball downloaded, contains an entry at {shown:?} with invalid type {kind:?}"
                ));
            }
        };
        if inside.rsplit('/').next() == Some(PACKAGE_SOURCE_LOCK) {
            continue;
        }
        if cfg!(windows) && is_windows_reserved_path(std::path::Path::new(&inside)) {
            return Err(format!(
                "`{shown}` appears to contain a reserved Windows path, it cannot be extracted on Windows"
            ));
        }
        if is_dir {
            paths.dir(&inside, &shown)?;
            continue;
        }
        if inside.is_empty() {
            return Err(format!(
                "invalid tarball downloaded, contains a file at {shown:?} which is the package's directory"
            ));
        }
        paths.file(&inside, &shown)?;
        let bits = entry
            .header()
            .mode()
            .map_err(|e| format!("failed to unpack entry at `{shown}`: {e}"))?;
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("failed to unpack entry at `{shown}`: {e}"))?;
        let mode = if bits & 0o111 != 0 { Mode::Executable } else { Mode::Regular };
        files.push(SourceFile { path: inside, bytes, mode });
    }
    Ok(files)
}

/// An entry's path under `prefix`, `/`-separated ("" for `prefix/` itself, a directory).
fn inside(raw: &[u8], shown: &str, prefix: &str, is_dir: bool) -> Result<String, String> {
    let refused = |why: &str| format!("invalid tarball downloaded, contains an entry at {shown:?} {why}");
    let path = std::str::from_utf8(raw).map_err(|_| refused("whose path is not UTF-8"))?;
    if path.starts_with('/') {
        return Err(refused("with an absolute path"));
    }
    let path = match is_dir {
        true => path.strip_suffix('/').unwrap_or(path),
        false => path,
    };
    let parts: Vec<&str> = path.split('/').collect();
    if let Some(part) = parts.iter().find(|it| matches!(**it, "" | "." | "..")) {
        return Err(match *part {
            "" => refused("with an empty part"),
            part => refused(&format!("with a `{part}` part")),
        });
    }
    if parts[0] != prefix {
        return Err(format!(
            "invalid tarball downloaded, contains a file at {shown:?} which isn't under {prefix:?}"
        ));
    }
    Ok(parts[1..].join("/"))
}

/// The paths unpacked so far: files, and directories named or under a file.
#[derive(Default)]
struct Paths {
    files: HashSet<String>,
    dirs: HashSet<String>,
}

impl Paths {
    fn file(&mut self, path: &str, shown: &str) -> Result<(), String> {
        if self.files.contains(path) {
            return Err(format!("invalid tarball downloaded, contains a file at {shown:?} twice"));
        }
        if self.dirs.contains(path) {
            return Err(format!(
                "invalid tarball downloaded, contains a file at {shown:?} where it has a directory"
            ));
        }
        self.parents(path, shown)?;
        self.files.insert(path.to_owned());
        Ok(())
    }

    /// A directory named twice is one directory, as cargo unpacks it.
    fn dir(&mut self, path: &str, shown: &str) -> Result<(), String> {
        if path.is_empty() {
            return Ok(());
        }
        if self.files.contains(path) {
            return Err(format!(
                "invalid tarball downloaded, contains a directory at {shown:?} where it has a file"
            ));
        }
        self.parents(path, shown)?;
        self.dirs.insert(path.to_owned());
        Ok(())
    }

    fn parents(&mut self, path: &str, shown: &str) -> Result<(), String> {
        for (at, _) in path.match_indices('/') {
            let parent = &path[..at];
            if self.files.contains(parent) {
                return Err(format!(
                    "invalid tarball downloaded, contains an entry at {shown:?} under the file `{parent}`"
                ));
            }
            self.dirs.insert(parent.to_owned());
        }
        Ok(())
    }
}
