//! [`Mounts`]: the spellings a run uses for its roots, and how a spelled path becomes a root and
//! a [`RelPath`]. A spelling is the run's view (the `CARGO_MANIFEST_DIR` a build script was
//! given, `/package` in the wasm host, the real directory a C compile was started in): it is
//! never in a key, a record or a read list.

#![blaze::sans_io]

use blaze_own::AVec;

use crate::path::{PathError, push_segments};
use crate::roots::{RootId, Roots};

/// One spelling and the root it names. `spelling` is absolute and canonical: `/` followed by
/// segments joined by single `/`, no trailing `/`, no `.` or `..` (`/` alone mounts everything).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mount<'g> {
    pub spelling: &'g [u8],
    pub root: RootId,
}

/// Why a set of mounts was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MountsError {
    /// The spelling at `at` is not absolute and canonical.
    Spelling { at: usize },
    /// The spelling at `at` names a root its [`Roots`] lacks.
    NoRoot { at: usize, root: RootId },
    /// Two mounts share one spelling.
    Twice { first: usize, second: usize },
}

/// Why a spelled path names no root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unmounted {
    /// The path itself is refused (not UTF-8, a NUL, too long, `..` past `/`).
    Path(PathError),
    /// A relative path and no working directory to read it from.
    NoCwd,
    /// No mount covers it: the run asked for a place it was not given.
    Outside,
}

/// A run's mounts over its roots.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mounts<'g> {
    mounts: &'g [Mount<'g>],
    /// Where a relative path is read from: one mount's spelling, or a directory beneath one
    /// (a build script's `CARGO_MANIFEST_DIR`); `None` refuses relative paths.
    cwd: Option<&'g [u8]>,
}

impl<'g> Mounts<'g> {
    pub fn new(mounts: &'g [Mount<'g>], cwd: Option<&'g [u8]>, roots: &Roots<'_>) -> Result<Mounts<'g>, MountsError> {
        for (at, mount) in mounts.iter().enumerate() {
            if !canonical_absolute(mount.spelling) || cwd.is_some_and(|cwd| !canonical_absolute(cwd)) {
                return Err(MountsError::Spelling { at });
            }
            if roots.get(mount.root).is_none() {
                return Err(MountsError::NoRoot { at, root: mount.root });
            }
            if let Some(first) = mounts[..at].iter().position(|m| m.spelling == mount.spelling) {
                return Err(MountsError::Twice { first, second: at });
            }
        }
        Ok(Mounts { mounts, cwd })
    }

    /// The working directory as the run spells it.
    pub fn cwd(&self) -> Option<&'g [u8]> {
        self.cwd
    }

    /// The mounts, in the order given.
    pub fn mounts(&self) -> &'g [Mount<'g>] {
        self.mounts
    }

    /// The spelling the run sees `root` at: what goes on a command line or a wire to the child
    /// (`-isystem /shelf/include`, a build script's `OUT_DIR`). The first mount of `root` in
    /// the order given; `None` for a root the run cannot address. A spelling is never in a key
    /// or a record: those hold the [`RootId`] and the root's name.
    pub fn spelling_of(&self, root: RootId) -> Option<&'g [u8]> {
        self.mounts.iter().find(|mount| mount.root == root).map(|mount| mount.spelling)
    }

    /// The root `path` is under, its relative path left in `out` (cleared first, canonical, at
    /// most `most` bytes). A relative `path` is read from the working directory; `.` and `..`
    /// are taken lexically (lib.rs); the longest mount covering the result wins.
    pub fn resolve(&self, path: &[u8], most: u32, out: &mut AVec<'_, u8>) -> Result<RootId, Unmounted> {
        out.clear();
        if core::str::from_utf8(path).is_err() {
            return Err(Unmounted::Path(PathError::NotUtf8));
        }
        if path.first() != Some(&b'/') {
            let cwd = self.cwd.ok_or(Unmounted::NoCwd)?;
            push_segments(cwd, out).map_err(Unmounted::Path)?;
        }
        push_segments(path, out).map_err(Unmounted::Path)?;
        let mut best: Option<(usize, RootId)> = None;
        for mount in self.mounts {
            let inner = &mount.spelling[1..];
            let covers = inner.is_empty()
                || out.as_slice() == inner
                || (out.starts_with(inner) && out.get(inner.len()) == Some(&b'/'));
            if covers && best.is_none_or(|(len, _)| inner.len() > len) {
                best = Some((inner.len(), mount.root));
            }
        }
        let (len, root) = best.ok_or(Unmounted::Outside)?;
        let cut = if len == 0 || out.len() == len { len } else { len + 1 };
        out.drain(..cut);
        if u32::try_from(out.len()).map_or(true, |n| n > most) {
            out.clear();
            return Err(Unmounted::Path(PathError::TooLong { most }));
        }
        Ok(root)
    }
}

impl Mounts<'static> {
    /// No mounts and no working directory: every path is [`Unmounted::Outside`] or
    /// [`Unmounted::NoCwd`] (a derive's run, which reads nothing).
    pub const NONE: Mounts<'static> = Mounts { mounts: &[], cwd: None };
}

fn canonical_absolute(spelling: &[u8]) -> bool {
    match spelling {
        b"/" => true,
        [b'/', rest @ ..] => {
            !rest.is_empty()
                && rest.split(|b| *b == b'/').all(|segment| !matches!(segment, b"" | b"." | b"..") && !segment.contains(&0))
                && core::str::from_utf8(rest).is_ok()
        }
        _ => false,
    }
}
