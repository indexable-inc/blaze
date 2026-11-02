//! The VFS's image as the daemon restores it (`crates/blaze-vfs/src/image.rs`, `Vfs::restore`):
//! its tree section mapped in place, its columns checked by `store::Base::within` and
//! `Base::check`, then read zero-copy by every list, read and follow. Fed through the public
//! API alone: the images are `Vfs::write_image`'s, of trees the input describes, each paired
//! with the journal rewritten to start from it, as the daemon's are.
//!
//! The image's CRCs would refuse any damage before the check is reached, so damaged sections
//! are written again into a new image (`Builder::add`, every section of the old one in its
//! order and schema), whose CRCs are good; the tree header keeps the name the journal's base
//! names unless the damage reaches it. The tree section's layout (the header's words, the
//! columns' order and element sizes) is `image.rs`'s, copied here because the crate keeps it
//! to itself; the round trip checks the copy.
//!
//! The input, read front to back (past its end every number is zero):
//! - one byte, the number of ops (`& 15`); each op a kind byte (`& 3`: a regular file, an
//!   executable, a symlink, a delete), a path's length (`& 31`) and its bytes, a length
//!   (`& 63`) and the file's bytes (a symlink's target). Each is committed to `main` alone; one
//!   the VFS refuses (a path that is not UTF-8 or not a path, a file in the way) is left out;
//! - then up to 32 damages of 12 bytes: a kind byte, a place byte, a little-endian `u16` offset
//!   and a `u64` value. Kind `& 3`: 0, header word `place % 28` becomes the value (a column's
//!   count or place, a count of the image, its name); 1, the bytes at `offset` of column
//!   `place % 10` (ids, shapes, entry names, entry nodes, entry modes, files, index, spans, name
//!   bytes, name index); 2, the bytes at `offset` of the disks section; 3, the bytes at
//!   `offset` anywhere in the tree section. For 1 to 3, `((kind >> 2) & 7) + 1` of the value's
//!   low bytes are written, cut at the end of what they land in. The blobs are never damaged.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - restoring never panics or hangs, and asks for at most `blaze_fuzz::allowed` of the image's
//!   and the journal's bytes at once;
//! - an image the check passes (`ImageUse::Used`, or `Resumed`): every head is a root the tree
//!   has, and every path under it lists, reads and follows; each file's bytes lie in the
//!   mapping, and no read is corrupt;
//! - an image refused says why in `Recovered::image`: `Malformed` with the check's reason (the
//!   column), `Corrupt` of the tree section for a column its header places past the section or
//!   misaligned (never a CRC), `Unnamed` for a header that no longer names the journal's image;
//!   only header damage gives anything but `Used` or `Malformed`. The VFS is then the journal's
//!   alone: what `Vfs::recover` of the journal gives, its report, heads and every file;
//! - round trip: an undamaged image is used as written, and restores to the tree that wrote it.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_core::{Completion, Done, Effect, FileName, Mapping};
use blaze_fuzzer::Target;
use blaze_ids::ContentId;
use blaze_image::{Builder, Cause, Image, MIN_GROUP_SHIFT, Section, SectionId};
use blaze_vfs::{ImageMark, ImageUse, Mode, Op, OpError, View, Vfs};

const TARGET: Target = Target {
    name: "vfs_image",
    check,
    seeds,
    dictionary: &[
        b"BZVFSTR1",
        b"/",
        b".",
        b"..",
        &[3],
        &[0xff, 0xff, 0xff, 0xff],
        &[0, 0, 0, 0, 1, 0, 0, 0],
        &[0, 0, 0, 0, 0, 0, 0, 0x80],
    ],
    max_len: 1 << 12,
    budget: Duration::from_secs(600),
};

const HEAD: &str = "main";

/// The most paths one head's walk visits: a damaged tree the check passes may name one
/// directory from many, and so hold more paths than the image has bytes.
const WALK: usize = 1 << 10;

/// The most damages one input makes.
const DAMAGES: usize = 32;

const MAGIC: u64 = u64::from_le_bytes(*b"BZVFSTR1");
const WORDS: usize = 28;
const W_MAGIC: usize = 0;
const W_NODES: usize = 1;
const W_ENTRIES: usize = 2;
const W_FILES: usize = 3;
const W_NAMES: usize = 4;
const W_NAME_BYTES: usize = 5;
const W_INDEX: usize = 6;
const W_NAME_INDEX: usize = 7;
const W_COLUMNS: usize = 9;
const COLUMNS: usize = 10;
const C_SHAPES: usize = 1;
const C_NAME_BYTES: usize = 8;

/// Each column's count (the header word that holds it) and element size, in the header's order.
const LAYOUT: [(usize, usize); COLUMNS] = [
    (W_NODES, 32),
    (W_NODES, 8),
    (W_ENTRIES, 4),
    (W_ENTRIES, 4),
    (W_ENTRIES, 1),
    (W_FILES, 16),
    (W_INDEX, 4),
    (W_NAMES, 8),
    (W_NAME_BYTES, 1),
    (W_NAME_INDEX, 4),
];

fn check(data: &[u8]) {
    let mut input = Input(data);
    let count = input.u8() & 15;
    let ops: Vec<Op> = (0..count).filter_map(|_| op(&mut input)).collect();
    let Imaged { mut vfs, mark, image, journal } = imaged(&ops);

    let mut sections = sections(&image);
    let of = |id: SectionId| sections.iter().position(|(it, _)| it.id == id);
    let tree_at = of(SectionId::VFS_TREE).expect("an image with a tree section");
    let disks_at = of(SectionId::VFS_DISKS).expect("an image with a disks section");
    let (tree, disks) = (&sections[tree_at].1, &sections[disks_at].1);
    let header = header(tree);
    assert_eq!(
        (header[W_MAGIC], header[W_NODES]),
        (MAGIC, mark.nodes),
        "the tree section's header is laid out as this target reads it"
    );
    let columns = columns(&header, tree.len());
    let (mut damaged_tree, mut damaged_disks) = (tree.clone(), disks.clone());
    for _ in 0..DAMAGES {
        if input.is_empty() {
            break;
        }
        Damage::read(&mut input).apply(&columns, &mut damaged_tree, &mut damaged_disks);
    }
    let header_damaged = damaged_tree[..WORDS * 8] != tree[..WORDS * 8];
    let tree_damaged = damaged_tree != *tree;
    let disks_damaged = damaged_disks != *disks;
    let image = match tree_damaged || disks_damaged {
        false => image,
        true => {
            sections[tree_at].1 = damaged_tree;
            sections[disks_at].1 = damaged_disks;
            rebuilt(&sections)
        }
    };

    let mapping = Mapping::copy(&image);
    let held = mapping.clone();
    let (largest, (mut back, mut found)) =
        blaze_fuzz::largest_allocation(|| Vfs::restore(journal_name(), 0, &journal, Some(mapping)));
    let allowed = blaze_fuzz::allowed(image.len() + journal.len());
    assert!(
        largest <= allowed,
        "restoring an image of {} bytes over a journal of {} asks for at most {allowed} at once, \
         not {largest}",
        image.len(),
        journal.len()
    );

    let used = match &found.image {
        ImageUse::Used { name, nodes, disks } => {
            assert_eq!((*name, *nodes), (mark.name, mark.nodes), "the image used is the one written");
            assert!(disks_damaged || disks.is_none(), "undamaged disks are used: {disks:?}");
            true
        }
        ImageUse::Resumed { .. } => {
            assert!(header_damaged, "an image whose header names no base is not resumed: {found:?}");
            true
        }
        ImageUse::Malformed(why) => {
            assert!(!why.is_empty(), "a refused tree says why");
            assert!(tree_damaged, "an undamaged tree is not refused: {why}");
            false
        }
        ImageUse::Corrupt(corrupt) => {
            assert!(header_damaged, "only a header places a column outside its section: {corrupt}");
            assert_eq!(corrupt.section, SectionId::VFS_TREE, "a column is of the tree section: {corrupt}");
            assert!(
                matches!(corrupt.cause, Cause::Bounds { .. } | Cause::Misaligned { .. }),
                "an image of good CRCs is corrupt only by a column's place: {corrupt}"
            );
            false
        }
        ImageUse::Unnamed { journal: named, .. } => {
            assert!(header_damaged, "only a header names another image: {found:?}");
            assert_eq!(*named, Some(mark.name), "the journal names the image written");
            false
        }
        ImageUse::None | ImageUse::Unopened(_) | ImageUse::Refused(_) => {
            panic!("an image of good CRCs with every section opens: {found:?}")
        }
    };

    if !used {
        let (mut alone, want) = Vfs::recover(journal_name(), 0, &journal);
        _ = std::mem::take(&mut found.image);
        assert_eq!(found, want, "a refused image leaves the journal's recovery");
        assert_eq!(heads(&back), heads(&alone), "a refused image leaves the journal's heads");
        for (head, root, _) in heads(&back) {
            assert_eq!(
                walk(&mut back, root, None),
                walk(&mut alone, root, None),
                "a refused image leaves the journal's files at {head}"
            );
        }
        return;
    }

    let mapped = held.bytes();
    for (head, root, sequence) in heads(&back) {
        assert!(back.has(root), "the restored head {head:?} is at a root the tree has");
        assert!(sequence.is_some(), "the restored head {head:?} has a sequence number");
        walk(&mut back, root, Some(mapped));
    }
    assert_eq!(back.corrupt(), None, "no read of the undamaged blobs is corrupt");

    if !tree_damaged && !disks_damaged {
        assert_eq!(
            found.image,
            ImageUse::Used { name: mark.name, nodes: mark.nodes, disks: None },
            "an undamaged image is used as written"
        );
        assert_eq!((found.skipped, found.heads_dropped), (0, 0), "an undamaged image drops nothing");
        assert_eq!(heads(&back), heads(&vfs), "an undamaged image restores the heads written");
        for (head, root, _) in heads(&vfs) {
            assert_eq!(
                walk(&mut back, root, Some(mapped)),
                walk(&mut vfs, root, None),
                "an undamaged image restores {head} as it was written"
            );
        }
    }
}

/// The input of one run, read front to back; past its end every number is zero.
struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The next `len` bytes, or what is left.
    fn bytes(&mut self, len: usize) -> &'a [u8] {
        let (taken, rest) = self.0.split_at(len.min(self.0.len()));
        self.0 = rest;
        taken
    }

    fn word<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0; N];
        let taken = self.bytes(N);
        out[..taken.len()].copy_from_slice(taken);
        out
    }

    fn u8(&mut self) -> u8 {
        self.word::<1>()[0]
    }
}

/// The next op of the input; `None` for a path that is not UTF-8.
fn op(input: &mut Input<'_>) -> Option<Op> {
    let kind = input.u8() & 3;
    let len = usize::from(input.u8() & 31);
    let path = std::str::from_utf8(input.bytes(len)).ok().map(str::to_owned);
    let len = usize::from(input.u8() & 63);
    let bytes = input.bytes(len).to_vec();
    let path = path?;
    let mode = match kind {
        0 => Mode::Regular,
        1 => Mode::Executable,
        2 => Mode::Symlink,
        _ => return Some(Op::Delete { path }),
    };
    Some(Op::Write { mode, path, bytes })
}

/// One overwrite of the image's tree or disks section, as the module's doc lays it out.
#[derive(Clone, Copy)]
struct Damage {
    kind: u8,
    place: u8,
    at: u16,
    value: u64,
}

impl Damage {
    fn read(input: &mut Input<'_>) -> Damage {
        Damage {
            kind: input.u8(),
            place: input.u8(),
            at: u16::from_le_bytes(input.word()),
            value: u64::from_le_bytes(input.word()),
        }
    }

    fn encode(self, out: &mut Vec<u8>) {
        out.push(self.kind);
        out.push(self.place);
        out.extend_from_slice(&self.at.to_le_bytes());
        out.extend_from_slice(&self.value.to_le_bytes());
    }

    /// Does the damage to `tree` and `disks`, whose columns are `columns` (from the undamaged
    /// header).
    fn apply(self, columns: &[(usize, usize); COLUMNS], tree: &mut [u8], disks: &mut [u8]) {
        let value = self.value.to_le_bytes();
        let value = &value[..usize::from((self.kind >> 2) & 7) + 1];
        match self.kind & 3 {
            0 => {
                let word = usize::from(self.place) % WORDS * 8;
                tree[word..word + 8].copy_from_slice(&self.value.to_le_bytes());
            }
            1 => {
                let (start, len) = columns[usize::from(self.place) % COLUMNS];
                overwrite(&mut tree[start..start + len], self.at, value);
            }
            2 => overwrite(disks, self.at, value),
            _ => overwrite(tree, self.at, value),
        }
    }
}

/// `value` written at `at` (modulo the length) of `bytes`, cut at their end.
fn overwrite(bytes: &mut [u8], at: u16, value: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let at = usize::from(at) % bytes.len();
    let len = value.len().min(bytes.len() - at);
    bytes[at..at + len].copy_from_slice(&value[..len]);
}

fn header(tree: &[u8]) -> [u64; WORDS] {
    std::array::from_fn(|word| {
        u64::from_le_bytes(tree[word * 8..word * 8 + 8].try_into().expect("8 bytes"))
    })
}

/// Where each column is in the tree section, `tree_len` long: its start and its length in bytes.
fn columns(header: &[u64; WORDS], tree_len: usize) -> [(usize, usize); COLUMNS] {
    std::array::from_fn(|column| {
        let (count, size) = LAYOUT[column];
        let start = usize::try_from(header[W_COLUMNS + column]).expect("a column's place");
        let len = usize::try_from(header[count]).expect("a column's count") * size;
        assert!(
            start + len <= tree_len,
            "column {column} of an image just written, {len} bytes at {start}, is in its section \
             of {tree_len}"
        );
        (start, len)
    })
}

fn journal_name() -> FileName {
    FileName::new("vfs/journal").expect("a file name")
}

/// A VFS, its image, and its journal rewritten to start from that image.
struct Imaged {
    vfs: Vfs,
    mark: ImageMark,
    image: Vec<u8>,
    journal: Vec<u8>,
}

/// `ops` committed to `main` one at a time and settled, the image written and made durable,
/// and the journal's rewrite onto it settled: what the daemon's disk holds after a stop.
fn imaged(ops: &[Op]) -> Imaged {
    let mut journal = Vec::new();
    let mut vfs = Vfs::recover(journal_name(), 0, &[]).0;
    for op in ops {
        let base = vfs.head(HEAD).unwrap_or(vfs.empty());
        _ = vfs.commit(HEAD, base, std::slice::from_ref(op));
    }
    if vfs.head(HEAD).is_none() {
        let empty = vfs.empty();
        vfs.set_head(HEAD, empty).expect("the empty root is the tree's");
    }
    settle(&mut vfs, &mut journal);
    let mut builder = Builder::with_group_shift(MIN_GROUP_SHIFT);
    let mark = vfs.write_image(&mut builder).expect("an image of a settled VFS");
    let image = builder.finish().expect("an image under the limit");
    assert!(vfs.image_durable(mark.name), "the image just written is the one staged");
    settle(&mut vfs, &mut journal);
    Imaged { vfs, mark, image, journal }
}

/// Every section of `image` and its bytes, in order.
fn sections(image: &[u8]) -> Vec<(Section, Vec<u8>)> {
    let opened = Image::open(Mapping::copy(image)).expect("an image just written opens");
    opened
        .sections()
        .iter()
        .map(|section| {
            let bytes = opened.whole(section).expect("an image just written is whole");
            (*section, bytes.to_vec())
        })
        .collect()
}

/// An image of `sections`, each as it is, its CRCs computed again.
fn rebuilt(sections: &[(Section, Vec<u8>)]) -> Vec<u8> {
    let mut builder = Builder::with_group_shift(MIN_GROUP_SHIFT);
    for (section, bytes) in sections {
        builder.add(section.id, section.schema, bytes);
    }
    builder.finish().expect("an image under the limit")
}

/// Completes every effect the journal asks for, as a disk holding `file` would, until it asks
/// for none.
fn settle(vfs: &mut Vfs, file: &mut Vec<u8>) {
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    while let Some(effect) = effects.pop() {
        let tag = effect.tag();
        let done = match effect {
            Effect::Append { bytes, .. } => {
                file.extend_from_slice(&bytes);
                Done::Appended
            }
            Effect::Sync { .. } => Done::Synced,
            Effect::Truncate { len, .. } => {
                file.truncate(usize::try_from(len).expect("a length within the file"));
                Done::Truncated
            }
            Effect::Write { bytes, .. } => {
                *file = bytes;
                Done::Written
            }
            other => panic!("the journal asks only for appends, syncs, cuts and rewrites: {other:?}"),
        };
        vfs.complete(Completion { tag, done }, &mut effects);
    }
}

/// Every head, its root and its sequence number, by name.
fn heads(vfs: &Vfs) -> Vec<(String, ContentId, Option<u64>)> {
    vfs.heads()
        .into_iter()
        .map(|(head, root)| (head.to_owned(), root, vfs.sequence(head)))
        .collect()
}

/// What a walk found at one path.
#[derive(PartialEq, Eq, Debug)]
enum Seen {
    Dir(Option<ContentId>, Vec<(String, bool)>),
    File(ContentId, Mode, Vec<u8>),
    /// Listed, but not found by its path: a name the damaged name index misplaces.
    Missing,
}

/// Lists, reads and follows every path under `root`, up to [`WALK`] of them, each file's bytes
/// checked to lie in `mapped` when given; what it found, in the walk's order.
fn walk(vfs: &mut Vfs, root: ContentId, mapped: Option<&[u8]>) -> Vec<(String, Seen)> {
    let view = vfs.open(View { root }).expect("a head's root opens");
    let mut paths = vec![String::new()];
    let mut seen = Vec::new();
    while let Some(path) = paths.pop() {
        if seen.len() == WALK {
            break;
        }
        let id = vfs.lookup(view, &path);
        match vfs.follow(view, &path) {
            Ok(Some(file)) => within(file.bytes, mapped, &path),
            Err(OpError::Corrupt { section, group }) => {
                panic!("{path:?}: following it reads group {group} of section {section:#x}, corrupt")
            }
            Ok(None) | Err(_) => {}
        }
        let found = match vfs.list(view, &path) {
            Some(entries) => {
                paths.extend(entries.iter().map(|(name, _)| match path.is_empty() {
                    true => name.clone(),
                    false => format!("{path}/{name}"),
                }));
                Seen::Dir(id, entries)
            }
            None => match vfs.read(view, &path) {
                Ok(Some(file)) => {
                    within(file.bytes, mapped, &path);
                    Seen::File(file.id, file.mode, file.bytes.to_vec())
                }
                Ok(None) => Seen::Missing,
                Err(corrupt) => panic!("{path:?}: a read is corrupt: {corrupt}"),
            },
        };
        seen.push((path, found));
    }
    vfs.close(view);
    seen
}

/// Checks that `bytes`, a file read at `path`, lie in `mapped`.
fn within(bytes: &[u8], mapped: Option<&[u8]>, path: &str) {
    let Some(mapped) = mapped else { return };
    let (at, start) = (bytes.as_ptr() as usize, mapped.as_ptr() as usize);
    assert!(
        bytes.is_empty() || (at >= start && at + bytes.len() <= start + mapped.len()),
        "{path:?}: a file's {} bytes are read from outside the image's mapping",
        bytes.len()
    );
}

/// Images of an empty tree, one file, nested directories (one emptied by a delete) and
/// symlinks (one out of the tree, one a loop), each undamaged and with a column's length, a
/// column's place and a name byte overwritten.
fn seeds() -> Vec<Vec<u8>> {
    let trees: [&[(u8, &str, &[u8])]; 4] = [
        &[],
        &[(0, "a", &b"hello"[..])],
        &[
            (0, "src/lib.rs", &b"fn a() {}"[..]),
            (1, "bin/run", &b"#!/bin/sh\n"[..]),
            (0, "src/x/y/z", &b"z"[..]),
            (3, "bin/run", &b""[..]),
        ],
        &[
            (0, "src/lib.rs", &b"fn a() {}"[..]),
            (2, "link", &b"src/lib.rs"[..]),
            (2, "src/up", &b"../link"[..]),
            (2, "out", &b"../../x"[..]),
            (2, "loop", &b"loop"[..]),
        ],
    ];
    let damages = [
        None,
        Some(Damage { kind: 0, place: W_ENTRIES as u8, at: 0, value: 1 << 40 }),
        Some(Damage { kind: 0, place: (W_COLUMNS + C_SHAPES) as u8, at: 0, value: 1 }),
        Some(Damage { kind: 1, place: C_NAME_BYTES as u8, at: 0, value: u64::from(b'/') }),
    ];
    let mut out = Vec::new();
    for tree in trees {
        for damage in damages {
            let mut input = vec![tree.len() as u8];
            for &(kind, path, bytes) in tree {
                input.push(kind);
                input.push(path.len() as u8);
                input.extend_from_slice(path.as_bytes());
                input.push(bytes.len() as u8);
                input.extend_from_slice(bytes);
            }
            if let Some(damage) = damage {
                damage.encode(&mut input);
            }
            out.push(input);
        }
    }
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
