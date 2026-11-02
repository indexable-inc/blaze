//! The tree in the daemon's image (docs/v2/plan/design/restore.md), opened in place: three
//! sections of `blaze-image`'s file, read where they are mapped. Nothing is re-interned or
//! hashed again on a start; a file's bytes are read (and their page groups checked) only when
//! something reads that file.
//!
//! - **`vfs tree`** ([`SectionId::VFS_TREE`]): a header of [`WORDS`] words (counts, where each
//!   column is, the hash naming the image, the base the journal started from when it was
//!   written), then the node columns as the store keeps them (IDs, shapes; each directory's
//!   entries as names, nodes and mode bytes; each file's place in the blobs), the names (spans
//!   and bytes) and the two indexes (by content ID and by name, open addressed, at most half
//!   full). Node `n` of the image is node `n` of the tree that wrote it. It is checked whole
//!   when opened (its CRC32Cs, then [`crate::store::Base::check`]'s pass over it), as every
//!   start reads it.
//! - **`vfs blobs`** ([`SectionId::VFS_BLOBS`]): every file's bytes, one after another. A file
//!   of the image is this section's range, its page groups checked the first time it is read.
//! - **`vfs disks`** ([`SectionId::VFS_DISKS`]): the watchers' metadata and journal cursors.
//!
//! The image is named by the hash of its tree section, which commits to every file's content
//! ID. The journal's base record names it once it is durable ([`Vfs::image_durable`]), so a
//! start reads only the journal's tail; [`Vfs::restore`] says in [`Recovered::image`] what it
//! used and why. A page group of the tree that fails its check leaves the image unused: the
//! journal is loaded alone ([`ImageUse::Corrupt`]).
//!
//! The image holds nodes, never heads: the journal names every head (a base record is only
//! written by a rewrite, which writes one record per head), so a head removed after the image
//! was written never comes back from it, and an image paired with another VFS's journal adds
//! only nodes, which are named by their content.

use std::{fmt, sync::Arc};

use blaze_core::{FileName, Mapping};
use blaze_format::{SchemaHash, schema};
use blaze_ids::{ContentId, Hasher};
use blaze_image::{
    Builder, Corrupt, Image, OpenError, Refused, Section as Placed, SectionId, bytes_of,
};

pub use blaze_bytes::Bytes;
use crate::{
    CommitError, OpError, Recovered, Vfs,
    disk::Disks,
    journal::{FromImage, Journal, Loaded, base_of},
    section::Section,
    store::{Base, FILE, Mapped, Source, Table, Writer, home, name_hash},
    tree::Tree,
};

pub const TREE_SCHEMA: SchemaHash = schema(
    "blaze v2 vfs tree 1: header [u64; 28]; ids ContentId; shapes [u32; 2]; entry names u32; \
     entry nodes u32; entry modes u8; files [u64; 2]; index u32; spans [u32; 2]; name bytes u8; \
     name index u32",
);
pub const BLOBS_SCHEMA: SchemaHash = schema("blaze v2 vfs blobs 1: file bytes");
pub const DISKS_SCHEMA: SchemaHash = schema("blaze v2 vfs disks 2: disk.rs's encoding");

const IMAGE_DOMAIN: &str = "blaze v2 vfs image 1";
const MAGIC: u64 = u64::from_le_bytes(*b"BZVFSTR1");

/// The header's words.
const WORDS: usize = 28;
const W_MAGIC: usize = 0;
const W_NODES: usize = 1;
const W_ENTRIES: usize = 2;
const W_FILES: usize = 3;
const W_NAMES: usize = 4;
const W_NAME_BYTES: usize = 5;
const W_INDEX: usize = 6;
const W_NAME_INDEX: usize = 7;
const W_BLOBS: usize = 8;
/// Where each of the [`COLUMNS`] columns starts in the section.
const W_COLUMNS: usize = 9;
const W_BASED: usize = 19;
const W_NAME: usize = 20;
const W_BASED_ON: usize = 24;
const _: () = assert!(W_BASED_ON + 4 == WORDS);

const C_IDS: usize = 0;
const C_SHAPES: usize = 1;
const C_ENTRY_NAMES: usize = 2;
const C_ENTRY_NODES: usize = 3;
const C_ENTRY_MODES: usize = 4;
const C_FILES: usize = 5;
const C_INDEX: usize = 6;
const C_SPANS: usize = 7;
const C_NAME_BYTES: usize = 8;
const C_NAME_INDEX: usize = 9;
const COLUMNS: usize = 10;
const _: () = assert!(W_COLUMNS + COLUMNS == W_BASED);

/// An image of the tree written into a [`Builder`]: what [`Vfs::image_durable`] is told once
/// the file holding it is durable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImageMark {
    /// The hash of the image's tree section, as the journal's base record names it.
    pub name: ContentId,
    pub nodes: u64,
}

/// Why [`Vfs::write_image`] wrote no image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageError {
    /// The journal is moving to another image, or names a node of its base a collection
    /// dropped, until a rewrite asked for or in flight is durable: flush, and write the image
    /// once it is. Nothing was written into the builder.
    Busy,
    /// A file of the image the tree was restored from failed its check, so its bytes are not
    /// there to write. The builder holds part of the VFS's sections: drop it.
    Corrupt(Corrupt),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Busy => f.write_str(
                "the VFS's journal is moving to another image or names nodes a collection \
                 dropped: flush, and write the image once its rewrite is durable",
            ),
            ImageError::Corrupt(corrupt) => {
                write!(f, "a file of the VFS's image cannot be written to the next: {corrupt}")
            }
        }
    }
}

impl std::error::Error for ImageError {}

/// What [`Vfs::restore`] did with the image it was given.
#[derive(Default, PartialEq, Eq, Debug)]
pub enum ImageUse {
    /// None was given: the journal was loaded alone.
    #[default]
    None,
    /// The tree was mapped in place and the journal's tail loaded over it. `disks` says why
    /// the watchers' metadata was not used (they then scan), `None` when it was.
    Used { name: ContentId, nodes: u64, disks: Option<DisksUnused> },
    /// The journal starts from the image this one was written over, or from none: a stop
    /// came after the image's rename and before the journal's rewrite. The tree was mapped in
    /// place and the whole journal loaded over it; the next flush rewrites the journal.
    Resumed { name: ContentId, nodes: u64, disks: Option<DisksUnused> },
    /// Not an image this blaze opens: the journal was loaded alone.
    Unopened(OpenError),
    /// The image has no VFS tree, or one of another layout: the journal was loaded alone.
    Refused(Refused),
    /// A page group of the VFS's tree failed its check: no byte of it was used, and the
    /// journal was loaded alone.
    Corrupt(Corrupt),
    /// The VFS's tree section is not a tree this blaze writes: the journal was loaded alone.
    Malformed(&'static str),
    /// The journal starts from another image (`journal`), or from none, than this one and
    /// the one it was written over: the journal was loaded alone.
    Unnamed { image: ContentId, journal: Option<ContentId> },
}

/// Why [`Vfs::restore`] did not use the image's watcher metadata (the `vfs disks` section).
/// [`DisksUnused::Refused`] is a section this blaze does not read, which the watchers' scan
/// replaces; the rest are an image that is broken.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DisksUnused {
    /// The image has no disks section, or one of another schema.
    Refused(Refused),
    /// A page group of the disks section failed its check.
    Corrupt(Corrupt),
    /// The disks section's range is outside the image's file.
    Outside,
    /// The disks section is not one this blaze writes: what is wrong with it.
    Malformed(&'static str),
}

impl fmt::Display for DisksUnused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisksUnused::Refused(refused) => write!(f, "the VFS image's disks section is refused: {refused}"),
            DisksUnused::Corrupt(corrupt) => write!(f, "the VFS image's disks section is corrupt: {corrupt}"),
            DisksUnused::Outside => f.write_str("the VFS image's disks section is outside its file"),
            DisksUnused::Malformed(what) => write!(f, "the VFS image's disks section: {what}"),
        }
    }
}

impl std::error::Error for DisksUnused {}

impl From<Corrupt> for OpError {
    fn from(corrupt: Corrupt) -> OpError {
        OpError::Corrupt { section: corrupt.section.0, group: corrupt.group }
    }
}

impl From<Corrupt> for CommitError {
    fn from(corrupt: Corrupt) -> CommitError {
        CommitError::Op(corrupt.into())
    }
}

impl Vfs {
    /// Writes the tree into `image` (its three sections: tree, blobs, disks) as it is now,
    /// and stages it: once the file holding it is durable, [`Vfs::image_durable`] has the
    /// journal's next flush rewrite the journal to start from it. Every node the tree has is
    /// written, numbered as it is, so a collection first makes the image compact.
    pub fn write_image(&mut self, image: &mut Builder) -> Result<ImageMark, ImageError> {
        if !self.journal.can_stage() {
            return Err(ImageError::Busy);
        }
        let mark = write_tree(self.tree.table(), self.journal.based_on(), image)
            .map_err(ImageError::Corrupt)?;
        // alloc: once per image written.
        let mut disks = Vec::new();
        self.disks.encode(&mut disks);
        image.add(SectionId::VFS_DISKS, DISKS_SCHEMA, &disks);
        self.journal.stage(mark.name, mark.nodes);
        Ok(mark)
    }

    /// The image `name` ([`Vfs::write_image`]'s) is durable: the next flush rewrites the
    /// journal to start from it. `false` (and nothing changes) for an image not the one last
    /// written.
    pub fn image_durable(&mut self, name: ContentId) -> bool {
        self.journal.durable(name)
    }

    /// The VFS of a journal's bytes over the daemon's image, its tree mapped in place: what
    /// [`Vfs::recover`] gives for the same history, reading only the journal's tail. `image`
    /// is the image's file as mapped; with none, or one the journal does not start from, the
    /// journal is loaded alone. [`Recovered::image`] says which, and why.
    pub fn restore(
        journal: FileName,
        tags: u64,
        bytes: &[u8],
        image: Option<Mapping>,
    ) -> (Vfs, Recovered) {
        let alone = |why: ImageUse| {
            // clone: the journal's name, once per start.
            let (vfs, mut found) = Vfs::recover(journal.clone(), tags, bytes);
            found.image = why;
            (vfs, found)
        };
        let Some(image) = image else { return alone(ImageUse::None) };
        let opened = match open(image) {
            Ok(opened) => opened,
            Err(why) => return alone(why),
        };
        let named = base_of(bytes);
        let fits = named == Some((opened.name, opened.nodes));
        let resumed = !fits && named.map(|it| it.0) == opened.based_on;
        if !fits && !resumed {
            return alone(ImageUse::Unnamed { image: opened.name, journal: named.map(|it| it.0) });
        }
        let Opened { tree, name, nodes, image, .. } = opened;
        let (disks, refused) = match open_disks(&image) {
            Ok(disks) => (disks, None),
            Err(why) => (Disks::default(), Some(why)),
        };
        let from = FromImage { tree, name, nodes, resumed };
        let Loaded { tree, heads, journal, mut found } =
            Journal::restore(journal, tags, bytes, from);
        found.image = match resumed {
            false => ImageUse::Used { name, nodes, disks: refused },
            true => ImageUse::Resumed { name, nodes, disks: refused },
        };
        // alloc: no views yet, once per start.
        (Vfs { tree, heads, views: Vec::new(), journal, disks }, found)
    }

    /// The first page group of the image's file bytes a read found corrupt, if any: its
    /// files there read as that error, never as their bytes, until written again.
    pub fn corrupt(&self) -> Option<Corrupt> {
        self.tree.table().mapped().and_then(|it| it.corrupt())
    }
}

/// The smallest index for `count` values at most half full.
fn places(count: usize) -> usize {
    (2 * (count + 1)).next_power_of_two().max(64)
}

fn words_of(id: &ContentId) -> [u64; 4] {
    std::array::from_fn(|at| u64::from_le_bytes(id.0[at * 8..at * 8 + 8].try_into().expect("8")))
}

fn id_of(words: &[u64]) -> ContentId {
    let mut id = [0; 32];
    for (at, word) in words.iter().enumerate() {
        id[at * 8..at * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    ContentId(id)
}

/// Writes the blobs and the tree of `table` into `image`, recording the image the journal
/// starts from (`based_on`); its name and node count.
fn write_tree(
    table: &Table,
    based_on: Option<ContentId>,
    image: &mut Builder,
) -> Result<ImageMark, Corrupt> {
    let nodes = table.len() as usize;
    // alloc: the image's columns, built once per image written (the edge); no read does.
    let mut ids: Vec<ContentId> = Vec::with_capacity(nodes);
    let mut shapes: Vec<[u32; 2]> = Vec::with_capacity(nodes);
    // alloc: as above.
    let (mut entry_names, mut entry_nodes, mut entry_modes) = (Vec::new(), Vec::new(), Vec::new());
    // alloc: as above.
    let mut files: Vec<[u64; 2]> = Vec::new();
    let blobs_len = {
        let mut blobs = image.section(SectionId::VFS_BLOBS, BLOBS_SCHEMA);
        for node in 0..nodes as u32 {
            let shape = table.shape(node);
            ids.push(*table.id(node));
            if shape.is_dir() {
                let entries = table.entries(shape);
                shapes.push([entry_names.len() as u32, shape.len]);
                entry_names.extend_from_slice(entries.names);
                entry_nodes.extend_from_slice(entries.nodes);
                entry_modes.extend_from_slice(entries.modes);
                continue;
            }
            let bytes = match table.source(shape) {
                Source::Heap(bytes) => bytes,
                Source::Mapped { at, len } => {
                    let mapped = table.mapped().expect("a file of an image in its table");
                    mapped.read(at, len)?
                }
            };
            shapes.push([files.len() as u32, FILE]);
            files.push([blobs.bytes(bytes), bytes.len() as u64]);
        }
        blobs.len()
    };

    let names = table.names();
    // alloc: as above.
    let (mut spans, mut name_bytes): (Vec<[u32; 2]>, Vec<u8>) = (Vec::new(), Vec::new());
    for name in 0..names.len() {
        let bytes = names.get(name);
        spans.push([name_bytes.len() as u32, bytes.len() as u32]);
        name_bytes.extend_from_slice(bytes);
    }
    // alloc: as above.
    let mut index = vec![0u32; places(nodes)];
    let mask = index.len() - 1;
    for (node, id) in ids.iter().enumerate() {
        let mut at = home(id) & mask;
        while index[at] != 0 {
            at = (at + 1) & mask;
        }
        index[at] = node as u32 + 1;
    }
    // alloc: as above.
    let mut name_index = vec![0u32; places(spans.len())];
    let mask = name_index.len() - 1;
    for name in 0..names.len() {
        let mut at = name_hash(names.get(name)) as usize & mask;
        while name_index[at] != 0 {
            at = (at + 1) & mask;
        }
        name_index[at] = name + 1;
    }

    let mut header = [0u64; WORDS];
    header[W_MAGIC] = MAGIC;
    header[W_NODES] = nodes as u64;
    header[W_ENTRIES] = entry_names.len() as u64;
    header[W_FILES] = files.len() as u64;
    header[W_NAMES] = spans.len() as u64;
    header[W_NAME_BYTES] = name_bytes.len() as u64;
    header[W_INDEX] = index.len() as u64;
    header[W_NAME_INDEX] = name_index.len() as u64;
    header[W_BLOBS] = blobs_len;
    if let Some(based_on) = &based_on {
        header[W_BASED] = 1;
        header[W_BASED_ON..].copy_from_slice(&words_of(based_on));
    }
    let mut section = image.section(SectionId::VFS_TREE, TREE_SCHEMA);
    section.slice(&header);
    let mut hasher = Hasher::new(IMAGE_DOMAIN);
    // Each column's place in the header, and its bytes in the name (the indexes follow
    // from the rest, so only their places are).
    let mut put = |column: usize, placed: u64, bytes: &[u8]| {
        header[W_COLUMNS + column] = placed;
        hasher.update(bytes);
    };
    put(C_IDS, section.slice(&ids), bytes_of(&ids));
    put(C_SHAPES, section.slice(&shapes), bytes_of(&shapes));
    put(C_ENTRY_NAMES, section.slice(&entry_names), bytes_of(&entry_names));
    put(C_ENTRY_NODES, section.slice(&entry_nodes), bytes_of(&entry_nodes));
    put(C_ENTRY_MODES, section.slice(&entry_modes), &entry_modes);
    put(C_FILES, section.slice(&files), bytes_of(&files));
    put(C_INDEX, section.slice(&index), &[]);
    put(C_SPANS, section.slice(&spans), bytes_of(&spans));
    put(C_NAME_BYTES, section.slice(&name_bytes), &name_bytes);
    put(C_NAME_INDEX, section.slice(&name_index), &[]);
    // And of everything else in the header.
    hasher.update(bytes_of(&header[..W_NAME])).update(bytes_of(&header[W_BASED_ON..]));
    let name = hasher.finish();
    header[W_NAME..W_BASED_ON].copy_from_slice(&words_of(&name));
    section.patch(0, bytes_of(&header));
    Ok(ImageMark { name, nodes: nodes as u64 })
}

/// An image's tree, opened in place.
struct Opened {
    tree: Tree,
    name: ContentId,
    nodes: u64,
    based_on: Option<ContentId>,
    image: Arc<Image<Mapping>>,
}

/// The tree of the image in `file`: its spine checked, its tree section checked whole and
/// viewed in place; its blobs not read.
fn open(file: Mapping) -> Result<Opened, ImageUse> {
    let image = Image::open(file).map_err(ImageUse::Unopened)?;
    let tree = image.section(SectionId::VFS_TREE, TREE_SCHEMA).map_err(ImageUse::Refused)?;
    let blobs = image.section(SectionId::VFS_BLOBS, BLOBS_SCHEMA).map_err(ImageUse::Refused)?;
    image.whole(&tree).map_err(ImageUse::Corrupt)?;
    // alloc: the image, once per start, shared by every file read from it.
    let image = Arc::new(image);
    let header = image.slice::<u64>(&tree, 0, WORDS as u64).map_err(ImageUse::Corrupt)?;
    if header[W_MAGIC] != MAGIC {
        return Err(ImageUse::Malformed("a VFS tree section without its magic"));
    }
    if header[W_BLOBS] != blobs.len || header[W_BASED] > 1 {
        return Err(ImageUse::Malformed("a VFS tree header that is not its image's"));
    }
    let base = view(&image, &tree, header).map_err(ImageUse::Corrupt)?;
    // alloc: once per start.
    let mapped = Arc::new(Mapped::new(Arc::clone(&image), blobs));
    let writer = Writer::restore(mapped, base).map_err(ImageUse::Malformed)?;
    Ok(Opened {
        tree: Tree::over(writer),
        name: id_of(&header[W_NAME..W_BASED_ON]),
        nodes: header[W_NODES],
        based_on: (header[W_BASED] == 1).then(|| id_of(&header[W_BASED_ON..])),
        image,
    })
}

/// The base's columns, in place.
fn view<'a>(
    image: &'a Image<Mapping>,
    tree: &Placed,
    header: &[u64],
) -> Result<Base<'a>, Corrupt> {
    let at = |column: usize| header[W_COLUMNS + column];
    let (nodes, entries) = (header[W_NODES], header[W_ENTRIES]);
    Ok(Base {
        ids: image.slice(tree, at(C_IDS), nodes)?,
        shapes: image.slice(tree, at(C_SHAPES), nodes)?,
        entry_names: image.slice(tree, at(C_ENTRY_NAMES), entries)?,
        entry_nodes: image.slice(tree, at(C_ENTRY_NODES), entries)?,
        entry_modes: image.slice(tree, at(C_ENTRY_MODES), entries)?,
        files: image.slice(tree, at(C_FILES), header[W_FILES])?,
        index: image.slice(tree, at(C_INDEX), header[W_INDEX])?,
        spans: image.slice(tree, at(C_SPANS), header[W_NAMES])?,
        name_bytes: image.slice(tree, at(C_NAME_BYTES), header[W_NAME_BYTES])?,
        name_index: image.slice(tree, at(C_NAME_INDEX), header[W_NAME_INDEX])?,
    })
}

/// The watchers' metadata and cursors of the image, its groups checked.
fn open_disks(image: &Image<Mapping>) -> Result<Disks, DisksUnused> {
    let placed = image.section(SectionId::VFS_DISKS, DISKS_SCHEMA).map_err(DisksUnused::Refused)?;
    let range = image.whole_range(&placed).map_err(DisksUnused::Corrupt)?;
    // clone: a count of the mapping, once per start.
    let section = Section::new(image.owner().clone(), range).ok_or(DisksUnused::Outside)?;
    Disks::open(section).map_err(DisksUnused::Malformed)
}

#[cfg(test)]
mod tests;
