//! [`Roots`]: what a run may see. Each root is read-only except the one output directory.

#![blaze::sans_io]

use crate::glob::Glob;
use crate::path::RelPath;

/// The most roots one run is given (a table has a cap: rule 18). A build script has its package,
/// its `links` dependencies' views and `OUT_DIR`; a C compile its sources, its include paths
/// and the shelf's headers. Sixty-four is above every case in tempo (inferred from the census
/// in build-scripts.md: none has more than a handful).
pub const MOST_ROOTS: usize = 64;

/// The digest of bytes as the store keys them (BLAKE3, 32 bytes): a file's content, a tree's
/// root, a listing's names ([`listing_digest`]). The caller supplies it: a digest-keyed tree
/// already names each file by it, and a driver reading a package directory hashes what it read.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Digest(pub [u8; 32]);

/// A root by its place in its [`Roots`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RootId(pub u8);

impl RootId {
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// What a root is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootKind<'g> {
    /// An immutable tree named by its digest (a package source in the store, the shelf's
    /// headers, a dependency's record view): every file beneath it may be read, and nothing
    /// beneath it changes while the digest is the same.
    Tree { tree: Digest },
    /// A directory on disk (a workspace member's checkout): only the files its globs admit may
    /// be read, and each read is keyed by its digest in the read list.
    Package { globs: &'g [Glob<'g>] },
    /// The one output directory (`OUT_DIR`, a compile's object folder): the only root a run
    /// writes. What a run reads back of its own writes is answered from its output, never
    /// listed: an output is not an input.
    Output,
}

/// One root: what it is and a name for a person (`"package tempo-alloy 0.1.0"`, `"shelf
/// headers"`): the name is in errors and in the roots' identity, never a host path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Root<'g> {
    pub name: &'g str,
    pub kind: RootKind<'g>,
}

impl Root<'_> {
    /// Whether the file `path` may be read under this root.
    pub fn admits_file(&self, path: RelPath<'_>) -> bool {
        match self.kind {
            RootKind::Tree { .. } => true,
            RootKind::Package { globs } => globs.iter().any(|glob| glob.matches(path)),
            RootKind::Output => false,
        }
    }

    /// Whether the directory `path` may be listed (or looked up) under this root: some glob
    /// reaches beneath it.
    pub fn admits_dir(&self, path: RelPath<'_>) -> bool {
        match self.kind {
            RootKind::Tree { .. } => true,
            RootKind::Package { globs } => globs.iter().any(|glob| glob.reaches(path)),
            RootKind::Output => false,
        }
    }
}

/// Why a set of roots was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootsError {
    TooMany { most: usize },
    /// Two output roots: a run writes one place.
    TwoOutputs { first: RootId, second: RootId },
    /// A package root with no globs admits nothing: leave it out.
    NoGlobs { root: RootId },
}

/// A run's roots: checked at construction (at most [`MOST_ROOTS`], at most one output).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Roots<'g> {
    roots: &'g [Root<'g>],
    output: Option<RootId>,
}

impl<'g> Roots<'g> {
    pub fn new(roots: &'g [Root<'g>]) -> Result<Roots<'g>, RootsError> {
        if roots.len() > MOST_ROOTS {
            return Err(RootsError::TooMany { most: MOST_ROOTS });
        }
        let mut output = None;
        for (at, root) in roots.iter().enumerate() {
            let id = RootId(u8::try_from(at).map_err(|_| RootsError::TooMany { most: MOST_ROOTS })?);
            match root.kind {
                RootKind::Output => match output {
                    Some(first) => return Err(RootsError::TwoOutputs { first, second: id }),
                    None => output = Some(id),
                },
                RootKind::Package { globs } if globs.is_empty() => return Err(RootsError::NoGlobs { root: id }),
                RootKind::Package { .. } | RootKind::Tree { .. } => {}
            }
        }
        Ok(Roots { roots, output })
    }

    pub fn get(&self, id: RootId) -> Option<&'g Root<'g>> {
        self.roots.get(id.index())
    }

    pub fn output(&self) -> Option<RootId> {
        self.output
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The roots' identity, for a key: each root's place, name, kind, tree digest and globs, in
    /// order. No host path is in it (a mount's spelling is not).
    pub fn digest(&self) -> Digest {
        let mut h = blaze_blake3::Hasher::new_derive_key("blaze-roots 2026-11-20 roots");
        h.update(&[u8::try_from(self.roots.len()).unwrap_or(u8::MAX)]);
        for root in self.roots {
            part(&mut h, root.name.as_bytes());
            match root.kind {
                RootKind::Tree { tree } => {
                    h.update(&[0]);
                    h.update(&tree.0);
                }
                RootKind::Package { globs } => {
                    h.update(&[1]);
                    h.update(&wide(globs.len()).to_le_bytes());
                    for glob in globs {
                        part(&mut h, glob.as_str().as_bytes());
                    }
                }
                RootKind::Output => {
                    h.update(&[2]);
                }
            }
        }
        Digest(h.finalize())
    }
}

impl Roots<'static> {
    /// No roots: a run that sees nothing (a derive macro: tokens in, tokens out).
    pub const NONE: Roots<'static> = Roots { roots: &[], output: None };
}

/// `bytes` with its length first, so two parts cannot run together.
pub(crate) fn part(h: &mut blaze_blake3::Hasher, bytes: &[u8]) {
    h.update(&wide(bytes.len()).to_le_bytes());
    h.update(bytes);
}

/// A length as the 64 bits a digest is fed (blaze's targets have a `usize` of at most 64 bits).
pub(crate) fn wide(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Why names are not a listing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotSorted {
    /// The first name out of order (or equal to the one before it).
    pub at: usize,
}

/// The digest of a directory's names, which must come sorted bytewise and once each (the order
/// is the listing's identity, never the file system's: rule 25).
pub fn listing_digest<'n>(names: impl IntoIterator<Item = &'n str>) -> Result<Digest, NotSorted> {
    let mut h = blaze_blake3::Hasher::new_derive_key("blaze-roots 2026-11-20 listing");
    let mut before: Option<&str> = None;
    for (at, name) in names.into_iter().enumerate() {
        if before.is_some_and(|before| before.as_bytes() >= name.as_bytes()) {
            return Err(NotSorted { at });
        }
        part(&mut h, name.as_bytes());
        before = Some(name);
    }
    Ok(Digest(h.finalize()))
}
