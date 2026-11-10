//! A path outside the view and every source, read from the disk where it is
//! ([`blaze_units::query::FetchKind::Local`]): a `path` dependency outside the checkout (in a
//! temporary directory beside it, on another volume), or a manifest its workspace search looks
//! for above it. cargo builds these, so blaze mounts them rather than refusing: each is a root
//! of its own in the VFS, keyed by its bytes as a fetched source is, so every key that reads it
//! covers its files. Called on an IO thread of the plane ([`crate::serve::work::Steps::local`]).
//!
//! A directory is read whole, its root `target/` and every `.git` left out as a checkout's view
//! leaves them ([`crate::serve::heads::excluded`]), and its canonical path is watched outside
//! views ([`blaze_vfs::outside::want`]): the job reads it again before a later build unless the
//! watch vouches nothing under it changed since it was read. One file, or a path that is not
//! there, is read again by every job.

#![forbid(unsafe_code)]

use std::path::Path;

use blaze_api::ApiError;
use blaze_vfs::Mode;

use super::git::{is_executable, read_tree};
use crate::serve::heads::excluded;
use crate::serve::work::{LocalRead, Source, SourceFile};

/// The most files one local mount holds: a directory with more is refused naming it, never
/// mounted in part (a `path` dependency pointing at a home directory, say).
const MOST_FILES: usize = 200_000;

/// What the load asked for at `at` (`probe`: only whether it is there): a directory's whole
/// tree; a `Cargo.toml` read for its contents, its package's whole directory; any other file
/// alone; nothing there, `at` as absent.
pub fn read(at: &str, probe: bool) -> Result<LocalRead, ApiError> {
    let path = Path::new(at);
    if !path.is_absolute() {
        return Err(ApiError::new(format!("`{at}` is under no mount of the view and is not an absolute path to read it from"), Some(at.to_owned())));
    }
    let manifest = !probe && path.file_name().is_some_and(|it| it == "Cargo.toml");
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && manifest => {
            let dir = match at.rsplit_once('/') {
                Some(("", _)) | None => "/",
                Some((dir, _)) => dir,
            };
            read_at(dir)
        }
        _ => read_at(at),
    }
}

/// Whether nothing is at `at` now, as [`read_at`] finds a path absent: what vouches for a
/// local mount read as absent ([`crate::serve::mounts::Mounts::stale_locals`]), which no
/// watch can (a watch of a directory above it would hear every file below).
pub fn absent(at: &str) -> bool {
    matches!(
        std::fs::metadata(at),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
    )
}

/// The path `at` read exactly where it is: a directory whole, a file alone, or absent. What a
/// local mount is read again as.
pub fn read_at(at: &str) -> Result<LocalRead, ApiError> {
    let path = Path::new(at);
    let failed = |what: String| ApiError::new(what, Some(at.to_owned()));
    let meta = match std::fs::metadata(path) {
        Ok(meta) => Some(meta),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => None,
        Err(error) => return Err(failed(format!("reading `{at}`, a path outside the view that the load reads: {error}"))),
    };
    let name = match at.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name,
        _ => return Err(failed(format!("`{at}` names no file or directory to mount"))),
    };
    match meta {
        Some(meta) if meta.is_dir() => {
            let canonical = std::fs::canonicalize(path).map_err(|error| failed(format!("resolving `{at}`: {error}")))?;
            let Some(watched) = canonical.to_str().map(str::to_owned) else {
                return Err(failed(format!("the directory `{at}` resolves to `{}`, which is not UTF-8", canonical.display())));
            };
            // Wanted first and the generation read before any file is: a check against it later
            // vouches only for what the watch heard from before the read on.
            blaze_vfs::outside::want(&canonical);
            let generation = blaze_vfs::outside::generation();
            let files = read_tree(path, excluded)
                .map_err(|error| failed(format!("reading the directory `{at}`, outside the view, that the load reads: {error:#}")))?;
            if files.len() > MOST_FILES {
                return Err(failed(format!(
                    "the directory `{at}`, outside the view, that the load reads holds {} files, more than the {MOST_FILES} a local mount holds",
                    files.len()
                )));
            }
            // The watch is of the resolved directory, and hears nothing of a link above `at`
            // pointed elsewhere: a directory named through one is read again by every job.
            let watched = (canonical == path).then_some(watched);
            Ok(LocalRead { at: at.to_owned(), source: Source { files, under: String::new() }, watched, generation })
        }
        Some(meta) => {
            let generation = blaze_vfs::outside::generation();
            let bytes = SourceFile::read(path, meta.len()).map_err(|error| failed(format!("reading `{at}`: {error}")))?;
            let mode = match is_executable(&meta) {
                true => Mode::Executable,
                false => Mode::Regular,
            };
            let file = SourceFile { path: name.to_owned(), bytes, mode };
            Ok(LocalRead { at: at.to_owned(), source: Source { files: vec![file], under: name.to_owned() }, watched: None, generation })
        }
        None => {
            let generation = blaze_vfs::outside::generation();
            // An empty tree: `at` reads as absent, as cargo finds it.
            Ok(LocalRead { at: at.to_owned(), source: Source { files: Vec::new(), under: name.to_owned() }, watched: None, generation })
        }
    }
}
