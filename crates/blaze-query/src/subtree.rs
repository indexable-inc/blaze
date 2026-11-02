//! The hash of everything below a directory of a view ([`crate::Cx::tree`]), one query per
//! directory, so an edit reruns only the directories above it and every other directory's memo
//! is reused by early cutoff.
//!
//! A directory's hash is a Merkle hash of its entries sorted by name ([`TreeHash`]), so
//! anything that hashes a tree off a view (a disk, a record) gets the same value by the same
//! type. With `package`, cargo's rule for a build script that printed no `rerun-if-changed`:
//! no name starting with `.`, at any depth, and no `target` at the top.

use blaze_format::{FormatError, Persist, SchemaHash, Writer};
use blaze_ids::{ContentId, Hasher};
use blaze_vfs::Mode;
use serde::{Deserialize, Serialize};

use crate::{Cancelled, Cx, Query, QueryKind, kinds};

const TREE_DOMAIN: &str = "blaze v2 db subtree 1";
const PACKAGE_DOMAIN: &str = "blaze v2 db package subtree 1";
const KEY_DOMAIN: &str = "blaze v2 db subtree key 1";

/// One directory's hash, made from its entries in name order: each kept name, then `d` and the
/// subdirectory's own hash, or `f`, the file's content ID and its git mode; `-` for an entry
/// that is listed but can't be read.
pub struct TreeHash {
    hasher: Hasher,
    package: bool,
    top: bool,
}

impl TreeHash {
    /// The hash of the directory asked for when `top`, or of one below it.
    pub fn new(package: bool, top: bool) -> TreeHash {
        TreeHash { hasher: Hasher::new(if package { PACKAGE_DOMAIN } else { TREE_DOMAIN }), package, top }
    }

    /// Whether the entry `name` is hashed: callers skip one that isn't, and all below it.
    pub fn keeps(&self, name: &str) -> bool {
        !(self.package && (name.starts_with('.') || (self.top && name == "target")))
    }

    pub fn dir(&mut self, name: &str, hash: Option<[u8; 32]>) {
        self.hasher.part(name.as_bytes());
        match hash {
            Some(hash) => _ = self.hasher.update(b"d").update(&hash),
            None => _ = self.hasher.update(b"-"),
        }
    }

    pub fn file(&mut self, name: &str, file: Option<(ContentId, Mode)>) {
        self.hasher.part(name.as_bytes());
        match file {
            Some((id, mode)) => _ = self.hasher.update(b"f").id(&id).update(&mode.git().to_le_bytes()),
            None => _ = self.hasher.update(b"-"),
        }
    }

    pub fn finish(&self) -> [u8; 32] {
        self.hasher.finish().0
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct SubtreeKey {
    /// The directory, from the view's root (`""` for the root), with no trailing `/`.
    pub(crate) dir: String,
    pub(crate) package: bool,
    /// Whether `dir` is the directory asked for, not one below it: only there does `package`
    /// leave out `target`.
    pub(crate) top: bool,
}

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct SubtreeRoot: 1 {
        hash: ContentId,
        present: bool,
    }
}

/// A directory's hash, `None` for no directory there.
pub(crate) struct Digest(pub(crate) Option<[u8; 32]>);

impl Persist for Digest {
    const SCHEMA: SchemaHash = <SubtreeRoot as blaze_format::Record>::SCHEMA;
    type Ref<'a> = Option<[u8; 32]>;
    fn write(&self, out: &mut Writer) -> u64 {
        out.push(&SubtreeRoot {
            hash: ContentId(self.0.unwrap_or([0; 32])),
            present: self.0.is_some(),
        })
    }
    fn read(buffer: &[u8]) -> Result<Option<[u8; 32]>, FormatError> {
        let root = blaze_format::open::<SubtreeRoot>(buffer)?;
        Ok(root.present.then_some(root.hash.0))
    }
}

pub(crate) struct Subtree;

impl Query for Subtree {
    const KIND: QueryKind = kinds::TREE;
    const NAME: &'static str = "subtree";
    const PRODUCER: ContentId = blaze_format::schema("blaze v2 db subtree query 1").0;
    type Key = SubtreeKey;
    type Value = Digest;

    fn input(key: &SubtreeKey) -> ContentId {
        Hasher::new(KEY_DOMAIN)
            .part(key.dir.as_bytes())
            .update(&[u8::from(key.package), u8::from(key.top)])
            .finish()
    }

    fn execute(cx: &mut Cx<'_>, key: &SubtreeKey) -> Result<Digest, Cancelled> {
        let Some(names) = cx.dir(&key.dir)? else { return Ok(Digest(None)) };
        let mut hash = TreeHash::new(key.package, key.top);
        // alloc: one path buffer per run, reused for every entry.
        let mut path = String::with_capacity(key.dir.len() + 64);
        for (name, is_dir) in names {
            if !hash.keeps(name) {
                continue;
            }
            path.clear();
            if !key.dir.is_empty() {
                path.push_str(&key.dir);
                path.push('/');
            }
            path.push_str(name);
            if *is_dir {
                // alloc: the child's key, only when this directory runs (an edit below it).
                let child = SubtreeKey { dir: path.clone(), package: key.package, top: false };
                let below = cx.get::<Subtree>(&child)?;
                hash.dir(name, below);
            } else {
                let file = cx.file(&path)?;
                hash.file(name, file.map(|file| (file.id, file.mode)));
            }
        }
        Ok(Digest(Some(hash.finish())))
    }
}
