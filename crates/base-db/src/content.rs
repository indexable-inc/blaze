//! The content IDs of the front end's inputs (docs/v2/plan/reference/crates.md, "IDs"; docs/v2/plan/design/model.md):
//! every input carries the [`ContentId`] of what it is, so a result can be keyed
//! by the hashes of what it read rather than by revision, and two views with equal inputs share
//! it. Only content is hashed, never a `FileId`, `Crate`, `SourceRootId` or other dense index: a
//! file is named by its path, another input by its own content ID.

use std::{cell::RefCell, io::Write as _, ops::Range};

use blaze_format::cursor::Put;
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{
    Frame,
    scratch::{self, Scratch},
};
use cfg::{CfgAtom, CfgOptions};
use crate::VfsPath;

/// A content ID as an input holds it. Only base-db makes one, from the value the input is set
/// with, so an input's ID and value never disagree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InputId(pub(crate) ContentId);

impl InputId {
    pub fn id(self) -> ContentId {
        self.0
    }
}

pub const CFG_OPTIONS_DOMAIN: &str = "blaze v2 cfg options 1";

/// The content ID of a set of cfg atoms: sorted by their bytes, so the order of the set does
/// not matter.
pub fn cfg_options_content(cfg: &CfgOptions) -> ContentId {
    // Every atom encoded into one buffer, then sorted as byte strings.
    // alloc: a crate's cfg, encoded when its content ID is made (setup).
    let mut bytes = Vec::new();
    // alloc: as above.
    let mut atoms: Vec<Range<usize>> = Vec::new();
    for atom in cfg {
        let start = bytes.len();
        match atom {
            CfgAtom::Flag(name) => {
                bytes.push(0);
                bytes.put_bytes_u64(name.as_str().as_bytes());
            }
            CfgAtom::KeyValue { key, value } => {
                bytes.push(1);
                bytes.put_bytes_u64(key.as_str().as_bytes());
                bytes.put_bytes_u64(value.as_str().as_bytes());
            }
        }
        atoms.push(start..bytes.len());
    }
    atoms.sort_unstable_by(|a, b| bytes[a.start..a.end].cmp(&bytes[b.start..b.end]));
    let mut hasher = Hasher::new(CFG_OPTIONS_DOMAIN);
    len(&mut hasher, atoms.len());
    for atom in atoms {
        hasher.part(&bytes[atom]);
    }
    hasher.finish()
}

/// A path as content, at its exact size: whether it is on disk (0) or virtual (1), then its text.
pub(crate) fn path_key(path: &VfsPath) -> Box<[u8]> {
    scratch::with_bytes(|key| {
        path_key_into(path, key);
        Box::from(&key[..]) // alloc: the key a source root's content keeps
    })
}

/// [`path_key`] appended to `out`, for keys that are only hashed.
pub(crate) fn path_key_into(path: &VfsPath, out: &mut Vec<u8>) {
    out.push(if path.as_path().is_some() { 0 } else { 1 });
    write!(out, "{path}").expect("writing to a Vec does not fail");
}

pub(crate) fn len(hasher: &mut Hasher, len: usize) {
    hasher.update(&(len as u64).to_le_bytes());
}

/// What [`Hasher::opt_part`] hashes, appended to `out`.
pub(crate) fn opt_bytes(out: &mut Vec<u8>, part: Option<&str>) {
    match part {
        None => out.push(0),
        Some(part) => {
            out.push(1);
            out.put_bytes_u64(part.as_bytes());
        }
    }
}

thread_local! {
    /// The IDs of the batch being hashed, kept across setter calls.
    static IDS: RefCell<Scratch<ContentId>> = const { RefCell::new(Scratch::new()) };
}

/// `ContentId::of(domain, input)` for each input, in the frame: the domain's key derived once
/// for all of them, the hasher's buffers and the result's the thread's.
pub(crate) fn hash_many<'f>(domain: &'static str, inputs: &[&[u8]], frame: &'f Frame) -> &'f [ContentId] {
    if inputs.is_empty() {
        return &[];
    }
    IDS.with_borrow_mut(|ids| {
        let ids = ids.take();
        ContentId::of_many_into(domain, inputs, ids);
        &*frame.alloc_slice_copy(ids)
    })
}

/// [`hash_many`] of `&buf[range]` for each range: the inputs laid out in one buffer.
pub(crate) fn hash_ranges<'f>(
    domain: &'static str,
    buf: &[u8],
    ranges: &[Range<usize>],
    frame: &'f Frame,
) -> &'f [ContentId] {
    let inputs = frame.alloc_slice_from_iter(ranges.iter().map(|range| &buf[range.start..range.end]));
    hash_many(domain, inputs, frame)
}
