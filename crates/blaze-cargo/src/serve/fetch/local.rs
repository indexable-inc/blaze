//! A path outside the view and every source, read from the disk where it is
//! ([`blaze_units::query::FetchKind::Local`]): a `path` dependency outside the checkout (in a
//! temporary directory beside it, on another volume), or a manifest its workspace search looks
//! for above it. cargo builds these, so blaze mounts them rather than refusing: each is a root
//! of its own in the VFS, keyed by its bytes as a fetched source is, so every key that reads it
//! covers its files. Called on an IO thread of the plane ([`crate::serve::work::Steps::local`]).
//!
//! A directory is read whole, its root `target/` and every `.git` left out as a checkout's view
//! leaves them ([`blaze_vfs::watch::fixed_out`]). Nothing is watched: every directory and file
//! read is a leaf with its `lstat` stamp, taken before anything is read
//! ([`crate::serve::work::LocalLeaf`]), and a later job reads the local again only when a
//! leaf's stamp no longer holds ([`blaze_vfs::outside::holds`]): a file written, made, removed
//! or renamed in any directory of it moves a stamp. One file is one leaf; a path that is not
//! there is one leaf whose stamp is [`blaze_core::Stamp::GONE`]. A local named through a link
//! has no leaves and is read again by every job: a link above it can be pointed elsewhere with
//! no leaf under it moving.
//!
//! The floor of a check: one `lstat` per directory and file of the local (the leaves), nothing
//! read; a check costs exactly that (ratio 1.0). Until 2026-11-14 a directory was watched,
//! which cost no syscall a check but a walk of the tree under it on the daemon's loop to set
//! the watch up (256,216 directories for one under `/root`), and killed the daemon.

#![forbid(unsafe_code)]

use std::{
    alloc::{Allocator, Global},
    path::Path,
};

use blaze_api::ApiError;
use blaze_vfs::Mode;

use super::git::{is_executable, read_tree};
use crate::serve::work::{LocalLeaf, LocalRead, Source, SourceFile};

/// The most files one local mount holds: a directory with more is refused naming it, never
/// mounted in part (a `path` dependency pointing at a home directory, say). Counted while its
/// leaves are stamped, before any file is read.
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

/// Every directory and file under `root` (itself included) that [`read_tree`] reads, with its
/// `lstat` stamp, in the order walked; `.git` and what `skip` names left out as `read_tree`
/// leaves them. Refused past [`MOST_FILES`] files, naming `at`.
fn leaves_of<A: Allocator + Clone>(
    at: &str,
    root: &Path,
    skip: impl Fn(&str) -> bool,
    alloc: A,
) -> Result<Vec<LocalLeaf, A>, ApiError> {
    let failed = |what: String| ApiError::new(what, Some(at.to_owned()));
    let path_of = |path: &Path| {
        path.to_str().map(str::to_owned).ok_or_else(|| failed(format!("`{}`, under `{at}`, is not UTF-8", path.display())))
    };
    let mut leaves = Vec::new_in(alloc);
    leaves.push(LocalLeaf { path: path_of(root)?, stamp: blaze_vfs::outside::lstat(root) });
    // The directories left to list: the walk's own, gone when it ends.
    let mut dirs = Vec::new_in(Global);
    dirs.push((root.to_owned(), String::new()));
    let mut files = 0usize;
    // Bounded: each turn takes one directory, and every directory is pushed once.
    while let Some((dir, under)) = dirs.pop() {
        let listed = std::fs::read_dir(&dir).map_err(|error| failed(format!("listing `{}`: {error}", dir.display())))?;
        for entry in listed {
            let entry = entry.map_err(|error| failed(format!("listing `{}`: {error}", dir.display())))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(failed(format!("`{}` is not UTF-8", entry.path().display())));
            };
            if name == ".git" {
                continue;
            }
            let rel = if under.is_empty() { name.to_owned() } else { format!("{under}/{name}") };
            if skip(&rel) {
                continue;
            }
            let at_path = entry.path();
            let stamp = blaze_vfs::outside::lstat(&at_path);
            if stamp.is_dir() {
                dirs.push((at_path.clone(), rel));
            } else {
                files += 1;
                if files > MOST_FILES {
                    return Err(failed(format!(
                        "the directory `{at}`, outside the view, that the load reads holds more than the {MOST_FILES} files a local mount holds"
                    )));
                }
            }
            leaves.push(LocalLeaf { path: path_of(&at_path)?, stamp });
        }
    }
    Ok(leaves)
}

/// The path `at` read exactly where it is: a directory whole, a file alone, or absent. What a
/// local mount is read again as.
pub fn read_at(at: &str) -> Result<LocalRead, ApiError> {
    let path = Path::new(at);
    let failed = |what: String| ApiError::new(what, Some(at.to_owned()));
    // The instant of the looks, before the first stamp: a lower bound of every one, so the
    // racy rule errs toward reading again.
    let looked_ns = blaze_core::system_wall_ns();
    let meta = match std::fs::metadata(path) {
        Ok(meta) => Some(meta),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => None,
        Err(error) => return Err(failed(format!("reading `{at}`, a path outside the view that the load reads: {error}"))),
    };
    let name = match at.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name,
        _ => return Err(failed(format!("`{at}` names no file or directory to mount"))),
    };
    // A path with a link in it has no leaves: a link above `at` pointed elsewhere moves no
    // stamp under it.
    let plain = || std::fs::canonicalize(path).is_ok_and(|it| it == path);
    match meta {
        Some(meta) if meta.is_dir() => {
            // Stamped before any file is read: a write after a stamp moves it, so the next job
            // reads the local again.
            let leaves = match plain() {
                true => Some(leaves_of(at, path, blaze_vfs::watch::fixed_out, Global)?.into_boxed_slice()),
                false => None,
            };
            let files = read_tree(path, blaze_vfs::watch::fixed_out)
                .map_err(|error| failed(format!("reading the directory `{at}`, outside the view, that the load reads: {error:#}")))?;
            if files.len() > MOST_FILES {
                return Err(failed(format!(
                    "the directory `{at}`, outside the view, that the load reads holds {} files, more than the {MOST_FILES} a local mount holds",
                    files.len()
                )));
            }
            Ok(LocalRead { at: at.to_owned(), source: Source { files, under: String::new() }, leaves, looked_ns })
        }
        Some(meta) => {
            let leaves = plain().then(|| vec![LocalLeaf { path: at.to_owned(), stamp: blaze_vfs::outside::lstat(path) }].into_boxed_slice());
            let bytes = SourceFile::read(path, meta.len()).map_err(|error| failed(format!("reading `{at}`: {error}")))?;
            let mode = match is_executable(&meta) {
                true => Mode::Executable,
                false => Mode::Regular,
            };
            let file = SourceFile::of(name.to_owned(), bytes, mode);
            Ok(LocalRead { at: at.to_owned(), source: Source { files: vec![file], under: name.to_owned() }, leaves, looked_ns })
        }
        None => {
            // An empty tree: `at` reads as absent, as cargo finds it, while its stamp is
            // `GONE` (asked of the whole path, so a part above it made counts too).
            // Made since the look found nothing: no leaf, so the next job reads it again.
            let stamp = blaze_vfs::outside::lstat(path);
            let leaves = stamp.is_gone().then(|| vec![LocalLeaf { path: at.to_owned(), stamp }].into_boxed_slice());
            Ok(LocalRead { at: at.to_owned(), source: Source { files: Vec::new(), under: name.to_owned() }, leaves, looked_ns })
        }
    }
}
