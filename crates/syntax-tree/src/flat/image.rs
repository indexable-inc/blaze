//! A tree as stored bytes read in place (base-db's store of library trees, kept in the
//! daemon's mapped image): its columns one after another behind a head saying where each is,
//! so reading a tree back is checking the head and lending each column from the bytes as they
//! are mapped. Nothing is decoded: [`FlatTree::from_image`] copies only the kept hashes (written
//! as they are made, so not in read-only bytes), the lazy nodes' list and the parsed parts'
//! errors, and makes the lazy nodes' slots.
//!
//! A record, little-endian, from a multiple of 8, every offset from its start:
//!
//! - the head: [`FIXED`] `u32`s ([`MAGIC`], the record's length in bytes, the elements,
//!   whether it has lazy nodes, the expander's data, how many parsed parts follow, how many
//!   errors they have, 0), then an offset and a count for each [`Column`];
//! - the columns, each from a multiple of 8, as the tree's block holds them: a `(u32, u32)` is
//!   two `u32`s, a `(u32, u16)` a `u32`, a `u16` and two bytes of 0, a kept hash two `u64`s
//!   (low, high; 0 for not made);
//! - each parsed part's tree, a record of this layout without lazy nodes, its offset and length
//!   in a [`Column::Parts`] row `[lazy node's index, offset, length, first error, errors, 0]`,
//!   its errors rows `[start, end, message offset, message length]` of [`Column::Errors`] with
//!   the messages' bytes in [`Column::Messages`].
//!
//! Reading checks, in time independent of the tree's size, that every column is inside the
//! record, aligned, and of the length the tree's element count makes it; of the lazy nodes and
//! parts, that each is in order and each part is its lazy node's kind over its range. The
//! bytes' integrity is the store's to check (the image's page groups): bytes that pass these
//! checks and are not a tree this wrote can make a read of the tree panic on an index out of
//! bounds, never read outside the bytes.

use std::{ops::Range, sync::OnceLock};

use blaze_bytes::{
    Bytes,
    le::{get_u32 as u32_at, get_u64 as u64_at},
};

use super::{
    block::{self, bytes_of, Block, Col, Viewable},
    Expand, FlatTree, Kept, Lazy, Part, Slot, MARK,
};
use crate::{Text, TextRange, TextSize};

/// What a record starts with; a new layout is a new value, as is [`IMAGE_LAYOUT`].
const MAGIC: u32 = u32::from_le_bytes(*b"bzt1");

/// The layout's version, for the store's schema: bumped with [`MAGIC`] on any change to what a
/// record holds or where.
pub const IMAGE_LAYOUT: u32 = 1;

/// The head's `u32`s before the columns' offsets and counts.
const FIXED: usize = 8;

/// The columns of a record, in the order they are written.
#[derive(Clone, Copy, Debug)]
enum Column {
    Nodes,
    KeptBits,
    KeptHashes,
    Words,
    Ranks,
    NodeParents,
    Far,
    Kinds,
    Big,
    Marks,
    KeptRanks,
    KeptElements,
    LazyNodes,
    Parts,
    Errors,
    Messages,
}

const COLUMNS: usize = Column::Messages as usize + 1;
/// The head's length in `u32`s.
const HEAD: usize = FIXED + 2 * COLUMNS;
/// A [`Column::Parts`] row's `u32`s.
const PART: usize = 6;
/// A [`Column::Errors`] row's `u32`s.
const ERROR: usize = 4;

/// Why stored bytes are not a tree: what was wrong, at which byte of the bytes read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageError {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (the tree record at byte {})", self.what, self.at)
    }
}

impl std::error::Error for ImageError {}

impl FlatTree {
    /// Appends the tree, with the lazy parts parsed so far and their errors, to `out` as a
    /// record read in place by [`FlatTree::from_image`]: from the next multiple of 8 of
    /// `out`'s length, which is where in `out` it is.
    pub fn write_image(&self, out: &mut Vec<u8>) -> Range<usize> {
        assert!(cfg!(target_endian = "little"), "a tree's record is little-endian, as its block");
        pad(out);
        let start = out.len();
        out.resize(start + HEAD * 4, 0);
        let mut head = [0u32; HEAD];
        let mut w = Writer { out: &mut *out, start, head: &mut head };
        let Kept { elements, bits, ranks: kept_ranks, hashes } = &self.kept;
        let nodes = &self.nodes;
        w.column(Column::Nodes, nodes.len(), |out| out.extend_from_slice(bytes_of(nodes)));
        w.column(Column::KeptBits, bits.len(), |out| out.extend_from_slice(bytes_of(bits)));
        w.column(Column::KeptHashes, hashes.len(), |out| {
            for slot in hashes.iter() {
                let hash = slot.value();
                out.extend_from_slice(&(hash as u64).to_le_bytes());
                out.extend_from_slice(&((hash >> 64) as u64).to_le_bytes());
            }
        });
        for (column, values) in [
            (Column::Words, &self.words),
            (Column::Ranks, &self.ranks),
            (Column::NodeParents, &self.node_parents),
        ] {
            w.column(column, values.len(), |out| out.extend_from_slice(bytes_of(values)));
        }
        for (column, pairs) in [(Column::Far, &self.far), (Column::Big, &self.big)] {
            w.column(column, pairs.len(), |out| {
                for &(a, b) in pairs.iter() {
                    out.extend_from_slice(&a.to_le_bytes());
                    out.extend_from_slice(&b.to_le_bytes());
                }
            });
        }
        w.column(Column::Kinds, self.kinds.len(), |out| {
            for &(at, kind) in self.kinds.iter() {
                out.extend_from_slice(&at.to_le_bytes());
                out.extend_from_slice(&kind.to_le_bytes());
                out.extend_from_slice(&[0, 0]);
            }
        });
        for (column, values) in [
            (Column::Marks, &self.marks),
            (Column::KeptRanks, kept_ranks),
            (Column::KeptElements, elements),
        ] {
            w.column(column, values.len(), |out| out.extend_from_slice(bytes_of(values)));
        }
        if let Some(lazy) = &self.lazy {
            w.head[3] = 1;
            w.head[4] = lazy.data;
            w.column(Column::LazyNodes, lazy.nodes.len(), |out| {
                out.extend_from_slice(bytes_of(&lazy.nodes))
            });
            // alloc: the rows of the parsed parts and their errors, once per tree stored
            let (mut parts, mut errors, mut messages) = (Vec::new(), Vec::new(), Vec::new());
            for (index, part) in lazy.parts.iter().enumerate() {
                let Some(part) = part.get() else { continue };
                assert!(part.tree.lazy.is_none(), "a part has no lazy nodes");
                let tree = part.tree.write_image(w.out);
                let first = errors.len() / ERROR;
                for (range, message) in part.errors.iter() {
                    errors.extend([u32::from(range.start()), u32::from(range.end())]);
                    errors.extend([to_u32(messages.len()), to_u32(message.len())]);
                    messages.extend_from_slice(message.as_bytes());
                }
                let (at, len) = (to_u32(tree.start - start), to_u32(tree.len()));
                parts.extend([to_u32(index), at, len, to_u32(first), to_u32(part.errors.len()), 0]);
            }
            w.head[5] = to_u32(parts.len() / PART);
            w.head[6] = to_u32(errors.len() / ERROR);
            let tables = [(Column::Parts, &parts, PART), (Column::Errors, &errors, ERROR)];
            for (column, rows, width) in tables {
                w.column(column, rows.len() / width, |out| out.extend_from_slice(bytes_of(rows)));
            }
            w.column(Column::Messages, messages.len(), |out| out.extend_from_slice(&messages));
        }
        pad(w.out);
        let end = w.out.len();
        head[0] = MAGIC;
        head[1] = to_u32(end - start);
        head[2] = self.len();
        out[start..start + HEAD * 4].copy_from_slice(bytes_of(&head));
        start..end
    }

    /// The tree of the record at byte `at` of `bytes` ([`FlatTree::write_image`]), over
    /// `source`, its columns read in place from `bytes`, which it holds; its unparsed lazy nodes
    /// are parsed by `expand`. An error says what about the bytes is not a tree.
    pub fn from_image(
        bytes: &Bytes,
        at: usize,
        source: Text,
        expand: Expand,
    ) -> Result<FlatTree, ImageError> {
        let wrong = |what| ImageError { at, what };
        if cfg!(target_endian = "big") {
            return Err(wrong("a big-endian target reads no stored tree"));
        }
        let mut head = [0u32; HEAD];
        for (n, word) in head.iter_mut().enumerate() {
            *word = u32_at(bytes, at + 4 * n).ok_or(wrong("a record past the end of its bytes"))?;
        }
        if head[0] != MAGIC {
            return Err(wrong("not a tree record of this layout"));
        }
        let len = head[1] as usize;
        if len < HEAD * 4 || at.checked_add(len).is_none_or(|end| end > bytes.len()) {
            return Err(wrong("a record's length past the end of its bytes"));
        }
        let record = Record { at, len, head: &head };
        let elements = head[2] as usize;
        let kept = record.count(Column::KeptHashes);
        // clone: a handle on the mapped bytes, which the tree's borrowed columns keep alive
        let mut block = Block::holding(block::room::<Slot>(0, kept), bytes.clone());
        let nodes: Col<u64> = record.view(&block, Column::Nodes)?;
        let bits: Col<u64> = record.view(&block, Column::KeptBits)?;
        let words: Col<u32> = record.view(&block, Column::Words)?;
        let ranks: Col<u32> = record.view(&block, Column::Ranks)?;
        let node_parents: Col<u32> = record.view(&block, Column::NodeParents)?;
        let far: Col<(u32, u32)> = record.view(&block, Column::Far)?;
        let kinds: Col<(u32, u16)> = record.view(&block, Column::Kinds)?;
        let big: Col<(u32, u32)> = record.view(&block, Column::Big)?;
        let marks: Col<u32> = record.view(&block, Column::Marks)?;
        let kept_ranks: Col<u32> = record.view(&block, Column::KeptRanks)?;
        let kept_elements: Col<u32> = record.view(&block, Column::KeptElements)?;
        let parents = match (nodes.last(), ranks.last()) {
            (Some(bits), Some(&before)) => before as usize + bits.count_ones() as usize,
            _ => 0,
        };
        let kept_words = match kept {
            0 => 0,
            _ => parents.div_ceil(64),
        };
        let fits = elements > 0
            && words.len() == elements
            && nodes.len() == elements.div_ceil(64)
            && ranks.len() == nodes.len()
            && node_parents.len() == parents
            && marks.len() == elements / MARK as usize + 1
            && kept_elements.len() == kept
            && bits.len() == kept_words
            && kept_ranks.len() == kept_words;
        if !fits {
            return Err(wrong("columns of other lengths than the tree's elements make them"));
        }
        let (hashes_at, _) = record.span(Column::KeptHashes, 16)?;
        let hashes = block.fill(kept, |n| {
            let at = hashes_at + 16 * n;
            let (low, high) = (u64_at(bytes, at), u64_at(bytes, at + 8));
            Slot::new(u128::from(low.unwrap_or(0)) | u128::from(high.unwrap_or(0)) << 64)
        });
        let lazy = match head[3] {
            0 if record.count(Column::LazyNodes) == 0 && head[5] == 0 => None,
            1 => Some(record.lazy(&block, head[4], expand, elements)?),
            _ => return Err(wrong("lazy nodes in a tree without an expander")),
        };
        let kept = Kept { elements: kept_elements, bits, ranks: kept_ranks, hashes };
        let tree = FlatTree {
            source,
            words,
            nodes,
            ranks,
            node_parents,
            far,
            kinds,
            big,
            marks,
            lazy,
            kept,
            block,
        };
        if tree.lazy.is_some() {
            record.parts(bytes, &tree, expand)?;
        }
        Ok(tree)
    }

    /// Whether its columns are read in place from stored bytes ([`FlatTree::from_image`]).
    pub fn is_read_in_place(&self) -> bool {
        self.block.is_held()
    }
}

/// A record being written: the head is written last, over the room left for it.
struct Writer<'a> {
    out: &'a mut Vec<u8>,
    start: usize,
    head: &'a mut [u32; HEAD],
}

impl Writer<'_> {
    /// Writes `column`, `count` values, from the next multiple of 8.
    fn column(&mut self, column: Column, count: usize, write: impl FnOnce(&mut Vec<u8>)) {
        pad(self.out);
        let at = to_u32(self.out.len() - self.start);
        write(self.out);
        self.head[FIXED + 2 * column as usize] = at;
        self.head[FIXED + 2 * column as usize + 1] = to_u32(count);
    }
}

fn pad(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(8), 0);
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("a tree record of under 4 GiB")
}
/// A record whose head was read: where it is in the bytes, and its head.
struct Record<'a> {
    at: usize,
    len: usize,
    head: &'a [u32; HEAD],
}

impl Record<'_> {
    fn wrong(&self, what: &'static str) -> ImageError {
        ImageError { at: self.at, what }
    }

    fn count(&self, column: Column) -> usize {
        self.head[FIXED + 2 * column as usize + 1] as usize
    }

    /// Where `column`'s values of `size` bytes start in the bytes, and how many there are:
    /// inside the record, or an error.
    fn span(&self, column: Column, size: usize) -> Result<(usize, usize), ImageError> {
        let (offset, count) = (self.head[FIXED + 2 * column as usize] as usize, self.count(column));
        let end = count.checked_mul(size).and_then(|it| it.checked_add(offset));
        match end {
            Some(end) if end <= self.len && (count == 0 || offset >= HEAD * 4) => {
                Ok((self.at + offset, count))
            }
            _ => Err(self.wrong("a column outside its record")),
        }
    }

    /// `column`, borrowed from the bytes `block` holds.
    fn view<T: Viewable>(&self, block: &Block, column: Column) -> Result<Col<T>, ImageError> {
        let (at, count) = self.span(column, size_of::<T>())?;
        block.borrow(at, count).ok_or(self.wrong("a column not aligned for its values"))
    }

    /// The lazy nodes, with a slot for each part.
    fn lazy(
        &self,
        block: &Block,
        data: u32,
        expand: Expand,
        elements: usize,
    ) -> Result<Box<Lazy>, ImageError> {
        let nodes: Col<u32> = self.view(block, Column::LazyNodes)?;
        let ascending = nodes.windows(2).all(|it| it[0] < it[1]);
        if nodes.is_empty() || !ascending || nodes.last().is_some_and(|&it| it as usize >= elements)
        {
            return Err(self.wrong("lazy nodes out of order or past the tree"));
        }
        // alloc: the lazy nodes and their parts' slots, once per tree read
        let parts = (0..nodes.len()).map(|_| OnceLock::new()).collect();
        // alloc: as above
        Ok(Box::new(Lazy { nodes: nodes.to_vec().into(), parts, expand, data }))
    }

    /// Reads the parsed parts into `tree`'s slots.
    fn parts(&self, bytes: &Bytes, tree: &FlatTree, expand: Expand) -> Result<(), ImageError> {
        let lazy = tree.lazy.as_deref().expect("a tree with lazy nodes");
        let (rows, parts) = self.span(Column::Parts, PART * 4)?;
        let (errors_at, errors) = self.span(Column::Errors, ERROR * 4)?;
        let (messages_at, messages) = self.span(Column::Messages, 1)?;
        if parts != self.head[5] as usize || errors != self.head[6] as usize {
            return Err(self.wrong("parts or errors of other counts than the head's"));
        }
        let messages = &bytes[messages_at..messages_at + messages];
        let row = |at: usize, n: usize| u32_at(bytes, at + 4 * n).map_or(0, |it| it as usize);
        for part in 0..parts {
            let at = rows + PART * 4 * part;
            let (index, offset, len, first, count) =
                (row(at, 0), row(at, 1), row(at, 2), row(at, 3), row(at, 4));
            let Some(&node) = lazy.nodes.get(index) else {
                return Err(self.wrong("a part of no lazy node"));
            };
            let inside = offset >= HEAD * 4 && offset.saturating_add(len) <= self.len;
            let stored_len = u32_at(bytes, self.at + offset + 4).map(|it| it as usize);
            if !inside || stored_len != Some(len) || first + count > errors {
                return Err(self.wrong("a part outside its record"));
            }
            // clone: a handle on the file's text, which a part shares
            let part = FlatTree::from_image(bytes, self.at + offset, tree.source.clone(), expand)?;
            let (kind, range) = (tree.kind(node), tree.range(node));
            let matches = part.lazy.is_none()
                && !part.is_token(0)
                && part.kind(0) == kind
                && part.range(0) == range;
            if !matches {
                return Err(self.wrong("a part that is not its lazy node over its text"));
            }
            // alloc: the part's errors, as a parse makes them
            let mut read = Vec::with_capacity(count);
            for error in first..first + count {
                let at = errors_at + ERROR * 4 * error;
                let (start, end) = (row(at, 0), row(at, 1));
                let message = messages.get(row(at, 2)..row(at, 2).saturating_add(row(at, 3)));
                let message = message.and_then(|it| std::str::from_utf8(it).ok());
                let (Some(message), true) = (message, start <= end) else {
                    return Err(self.wrong("an error of a part that is not one"));
                };
                let (start, end) = (TextSize::from(start as u32), TextSize::from(end as u32));
                let error = TextRange::new(start, end);
                if !range.contains_range(error) {
                    return Err(self.wrong("an error of a part outside it"));
                }
                read.push((error, Box::from(message)));
            }
            // alloc: a part's tree, shared as a parsed part's is, once per stored part read
            let part = Part { tree: std::sync::Arc::new(part), errors: read.into() };
            if lazy.parts[index].set(part).is_err() {
                return Err(self.wrong("a part stored twice"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use blaze_bytes::Bytes;
    use blaze_ids::rng::SplitMix64;

    use super::{super::tests as flat, FlatTree, ImageError, HEAD, MAGIC};

    /// Every column of `tree`, copied out, with the kept hashes.
    #[allow(clippy::type_complexity)]
    fn columns(
        tree: &FlatTree,
    ) -> (Vec<Vec<u32>>, Vec<Vec<u64>>, Vec<Vec<(u32, u32)>>, Vec<(u32, u16)>, Vec<u128>) {
        let kept = &tree.kept;
        let words = [&tree.words, &tree.ranks, &tree.node_parents, &tree.marks];
        (
            words.into_iter().chain([&kept.elements, &kept.ranks]).map(|it| it.to_vec()).collect(),
            vec![tree.nodes.to_vec(), kept.bits.to_vec()],
            vec![tree.far.to_vec(), tree.big.to_vec()],
            tree.kinds.to_vec(),
            kept.hashes.iter().map(|it| it.value()).collect(),
        )
    }

    /// `tree` written at byte `skew` (0 or 8) of fresh bytes.
    fn stored(tree: &FlatTree, skew: usize) -> (Bytes, usize) {
        let mut out = vec![0u8; skew];
        let at = tree.write_image(&mut out).start;
        (Bytes::from(Arc::<[u8]>::from(out)), at)
    }

    fn read(tree: &FlatTree, bytes: &Bytes, at: usize) -> Result<FlatTree, ImageError> {
        FlatTree::from_image(bytes, at, tree.source.clone(), flat::expand)
    }

    /// A tree read in place is the tree written: every column, the kept hashes made before it
    /// was written, every element's kind, range and parent, the same text.
    fn check_same(read: &FlatTree, tree: &FlatTree, seed: u64) {
        assert!(read.is_read_in_place(), "seed {seed}");
        flat::same_arrays(read, tree, seed);
        assert_eq!(columns(read), columns(tree), "seed {seed}");
        for at in 0..tree.len() {
            let what = (tree.kind(at), tree.range(at), tree.parent(at), tree.is_token(at));
            assert_eq!((read.kind(at), read.range(at), read.parent(at), read.is_token(at)), what);
        }
        assert!(read.source.ptr_eq(&tree.source), "seed {seed}: over the same source");
    }

    /// Trees of every shape the tests build, big ones past what a word holds included, with
    /// some hashes made: each is read back from its record as written, in place.
    #[test]
    fn a_tree_reads_back_from_its_record_as_written() {
        for seed in 0..504 {
            let shape = match seed >= 500 {
                true => flat::big_shape(seed),
                false => flat::random_shape(seed),
            };
            let tree = flat::build_flat(&shape, true);
            if seed % 2 == 0 {
                let _ = tree.subtree_hash(0);
            }
            for skew in [0, 8] {
                let (bytes, at) = stored(&tree, skew);
                let read = read(&tree, &bytes, at).unwrap_or_else(|it| panic!("seed {seed}: {it}"));
                check_same(&read, &tree, seed);
                assert_eq!(read.subtree_hash(0), tree.subtree_hash(0), "seed {seed}");
                #[allow(clippy::redundant_clone, reason = "the copy is the point: a clone packs a tree read in place into a block of its own")]
                let copy = read.clone();
                assert!(!copy.is_read_in_place(), "a copy is packed into a block of its own");
                flat::same_arrays(&copy, &tree, seed);
            }
        }
    }

    /// An outline with some parts parsed reads back with those parts parsed, the same trees
    /// and errors, and the rest unparsed until asked for, then parsed as the original's.
    #[test]
    fn an_outline_reads_back_with_the_parts_parsed_so_far() {
        for seed in 0..300 {
            let mut shape = flat::random_shape(seed);
            flat::make_lazy(&mut shape, &mut SplitMix64::new(seed ^ 0x1a2b));
            let tree = flat::build_outline(&shape, flat::expand);
            let lazies: Vec<u32> = tree.lazies().collect();
            if lazies.is_empty() {
                continue;
            }
            let mut rng = SplitMix64::new(seed);
            let parsed: Vec<u32> = lazies.iter().copied().filter(|_| rng.below(2) == 0).collect();
            for &at in &parsed {
                let _ = tree.part(at);
            }
            let (bytes, at) = stored(&tree, 0);
            let read = read(&tree, &bytes, at).unwrap_or_else(|it| panic!("seed {seed}: {it}"));
            check_same(&read, &tree, seed);
            for &at in &lazies {
                assert_eq!(read.is_parsed(at), parsed.contains(&at), "seed {seed}: {at}");
                let (part, original) = (read.part(at), tree.part(at));
                assert_eq!(part.is_read_in_place(), parsed.contains(&at), "seed {seed}: {at}");
                flat::same_arrays(part, original, seed);
                assert!(part.source.ptr_eq(&read.source), "seed {seed}: the part shares it");
                assert_eq!(read.part_errors(at), tree.part_errors(at), "seed {seed}: {at}");
            }
        }
    }

    /// Reading a tree allocates its slots, not a copy of its columns: the stored bytes'
    /// owner and one block (for the kept hashes), and for an outline its lazy nodes and slots,
    /// and per parsed part its tree's two, the part and its errors.
    #[test]
    fn a_tree_reads_in_place() {
        for seed in 500..504 {
            let tree = flat::build_flat(&flat::big_shape(seed), true);
            let (bytes, at) = stored(&tree, 0);
            let (read, used) = blaze_alloc_count::measure(|| read(&tree, &bytes, at));
            let read = read.expect("a tree");
            let block = usize::from(!read.kept.elements.is_empty());
            assert_eq!(used.allocations as usize, 1 + block, "seed {seed}: {} bytes", used.bytes);
            assert!((used.bytes as usize) < 64 + read.kept.elements.len() * 16, "seed {seed}");
        }
    }

    /// A record cut short, of another layout, with a column of another length or past the
    /// record, or at an offset its columns are not aligned at, is refused with what is wrong.
    #[test]
    fn bytes_that_are_not_a_tree_are_refused() {
        let tree = flat::build_flat(&flat::random_shape(7), true);
        let mut out = Vec::new();
        let record = tree.write_image(&mut out);
        let refused = |bytes: Vec<u8>, at: usize| {
            let bytes = Bytes::from(Arc::<[u8]>::from(bytes));
            read(&tree, &bytes, at).expect_err("refused").what
        };
        assert_eq!(refused(out[..HEAD * 4 - 1].to_vec(), 0), "a record past the end of its bytes");
        let short = refused(out[..record.end - 8].to_vec(), 0);
        assert_eq!(short, "a record's length past the end of its bytes");
        let mut other = out.clone();
        other[..4].copy_from_slice(&(MAGIC + 1).to_le_bytes());
        assert_eq!(refused(other, 0), "not a tree record of this layout");
        let mut more = out.clone();
        more[8..12].copy_from_slice(&(tree.len() + 64).to_le_bytes());
        assert!(refused(more, 0).contains("lengths"));
        let mut past = out.clone();
        let words = 4 * (super::FIXED + 2 * super::Column::Words as usize + 1);
        past[words..words + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(refused(past, 0), "a column outside its record");
        let mut skewed = vec![0u8; 4];
        skewed.extend_from_slice(&out);
        assert_eq!(refused(skewed, 4), "a column not aligned for its values");
        let whole = Bytes::from(Arc::<[u8]>::from(out));
        assert_eq!(read(&tree, &whole, 0).map(|it| it.len()), Ok(tree.len()));
    }

    /// A part whose root is not its lazy node over its text is refused.
    #[test]
    fn a_part_that_is_not_its_lazy_node_is_refused() {
        for seed in 0..200 {
            let mut shape = flat::random_shape(seed);
            flat::make_lazy(&mut shape, &mut SplitMix64::new(seed ^ 0x1a2b));
            let tree = flat::build_outline(&shape, flat::expand);
            let Some(at) = tree.lazies().next() else { continue };
            let _ = tree.part(at);
            let (bytes, start) = stored(&tree, 0);
            let mut out = bytes.to_vec();
            let word = |out: &[u8], at: usize| super::u32_at(out, at).unwrap() as usize;
            let rows = word(&out, 4 * (super::FIXED + 2 * super::Column::Parts as usize));
            let tree_at = word(&out, rows + 4);
            // The part's root's first mark: its start, moved one byte on.
            let marks = tree_at + 4 * (super::FIXED + 2 * super::Column::Marks as usize);
            let marks = tree_at + word(&out, marks);
            let mark = word(&out, marks) as u32;
            out[marks..marks + 4].copy_from_slice(&(mark + 1).to_le_bytes());
            let bytes = Bytes::from(Arc::<[u8]>::from(out));
            let what = read(&tree, &bytes, start).expect_err("refused").what;
            assert_eq!(what, "a part that is not its lazy node over its text", "seed {seed}");
            return;
        }
        panic!("a seed with a lazy node");
    }
}
