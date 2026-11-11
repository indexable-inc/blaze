//! The identity of an image's file, kept as the file is patched
//! (docs/v2/plan/design/patch-in-place.md).
//!
//! A Merkle tree over the file's [`BLOCK`]-byte blocks, the last padded with zeros: a leaf is
//! the hash of a block, a node the hash of its two children, and the root binds the top node to
//! the file's length. A patch that writes `k` blocks of a file of `n` rehashes those `k` blocks
//! and the `k * log2(n)` nodes above them, never the file. It is a sound identity because it is
//! a hash of the whole content: two files with one root have, by the collision resistance of
//! the hash, the same length (the root says it) and the same top node, so the same children all
//! the way down, so the same blocks. Leaves, nodes and roots are hashed in three domains, so
//! none is taken for another. An XOR of block hashes would update as cheaply and is not one: a
//! set of blocks XORing to a chosen value is found by linear algebra.
//!
//! An image is mostly room nothing wrote, so the tree is sparse: a subtree of zeros has a
//! hash known ahead ([`Digest::zeros`]) and no node kept. What is kept is canonical (a node
//! equal to the zero subtree's is not kept), so two digests of one file hold the same nodes.

#![forbid(unsafe_code)]

use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_ids::Hasher;

/// The bytes a leaf covers.
pub const BLOCK: usize = 4096;

/// A leaf, a node or a root.
pub type Hash = [u8; 32];

const LEAF: &str = "blaze v2 link file block 1";
const NODE: &str = "blaze v2 link file node 1";
const ROOT: &str = "blaze v2 link file 1";

/// The levels a tree over any file has: 2^52 blocks of 4 KiB are 2^64 bytes.
const LEVELS: usize = 53;

fn leaf(block: &[u8; BLOCK]) -> Hash {
    let mut hasher = Hasher::new(LEAF);
    hasher.update(block);
    hasher.finish().0
}

fn node(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = Hasher::new(NODE);
    hasher.update(left).update(right);
    hasher.finish().0
}

/// A node's place as one word: its level above the leaves, then its index in the level.
fn place(level: usize, index: u64) -> u64 {
    (level as u64) << 56 | index
}

/// The Merkle tree of one file. The invariant: for every level up to [`Digest::height`], a
/// node kept is the hash of its children, and a node not kept is the zero subtree's.
pub struct Digest {
    len: u64,
    nodes: HashMap<u64, Hash, Global>,
    /// `zeros[l]`: the hash of a subtree of `2^l` zero blocks.
    zeros: [Hash; LEVELS],
    /// The leaves an update set, then level by level their parents: scratch kept across
    /// updates.
    dirty: Vec<u64>,
    /// The planted bug of `the_oracle_catches_a_block_not_rehashed`: an update skips its last
    /// block.
    #[cfg(test)]
    pub(crate) plant_skips_a_block: bool,
}

impl Default for Digest {
    fn default() -> Digest {
        Digest::new()
    }
}

impl Digest {
    /// The digest of the empty file.
    pub fn new() -> Digest {
        let mut zeros = [[0; 32]; LEVELS];
        zeros[0] = leaf(&[0; BLOCK]);
        for level in 1..LEVELS {
            zeros[level] = node(&zeros[level - 1], &zeros[level - 1]);
        }
        Digest {
            len: 0,
            // alloc: once per image, the tree's nodes, which live as long as the image
            nodes: HashMap::new_in(Global),
            zeros,
            // alloc: none until an update, then kept
            dirty: Vec::new(),
            #[cfg(test)]
            plant_skips_a_block: false,
        }
    }

    /// The digest of a file holding `bytes`: every block hashed. What a patched digest is
    /// checked against, and what a file read back after a crash is hashed with.
    pub fn of(bytes: &[u8]) -> Digest {
        let mut digest = Digest::new();
        let mut read = |block: u64, out: &mut [u8; BLOCK]| {
            let from = (block as usize).saturating_mul(BLOCK).min(bytes.len());
            let to = from.saturating_add(BLOCK).min(bytes.len());
            out[..to - from].copy_from_slice(&bytes[from..to]);
        };
        digest.resize(bytes.len() as u64, &mut read);
        digest.update(&[(0, bytes.len() as u64)], &mut read);
        digest
    }

    /// The file's length.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many blocks the file has, the last maybe partial.
    fn blocks(len: u64) -> u64 {
        len.div_ceil(BLOCK as u64)
    }

    /// The level of the top node over `blocks` blocks: the least `h` with `2^h >= blocks`.
    fn height_of(blocks: u64) -> usize {
        match blocks {
            0 | 1 => 0,
            n => (u64::BITS - (n - 1).leading_zeros()) as usize,
        }
    }

    /// The level of the tree's top node.
    pub fn height(&self) -> usize {
        Digest::height_of(Digest::blocks(self.len))
    }

    fn get(&self, level: usize, index: u64) -> Hash {
        self.nodes.get(&place(level, index)).copied().unwrap_or(self.zeros[level])
    }

    /// Keeps `hash` as the node at `level` and `index`, canonically: a zero subtree's is not
    /// kept.
    fn set(&mut self, level: usize, index: u64, hash: Hash) {
        if hash == self.zeros[level] {
            self.nodes.remove(&place(level, index));
        } else {
            self.nodes.insert(place(level, index), hash);
        }
    }

    /// The file's identity: of its length and its top node.
    pub fn root(&self) -> Hash {
        let mut hasher = Hasher::new(ROOT);
        hasher.update(&self.len.to_le_bytes()).update(&self.get(self.height(), 0));
        hasher.finish().0
    }

    /// How many nodes are kept: what the digest holds in memory, 40 bytes each.
    pub fn nodes(&self) -> usize {
        self.nodes.len()
    }

    /// The file is now `len` bytes. A shorter file loses the blocks past its end, and its new
    /// last block is read again (what is past the end reads as zeros); a longer one gains
    /// blocks of zeros until [`Digest::update`] says what was written there. `read` fills a
    /// zeroed block with the file's bytes of that block, those before `len`.
    pub fn resize(&mut self, len: u64, read: &mut impl FnMut(u64, &mut [u8; BLOCK])) {
        let (old, new) = (Digest::blocks(self.len), Digest::blocks(len));
        self.dirty.clear();
        if len < self.len {
            // Every leaf kept past the new end goes, through the tree as it stands (the taller
            // one), so no node above the new top is left saying what the file no longer holds.
            let mut gone: Vec<u64> = self // alloc: a file made shorter, which a splice does not
                .nodes
                .keys()
                .copied()
                .filter(|at| at >> 56 == 0 && *at >= new)
                .collect();
            gone.sort_unstable();
            for &index in &gone {
                self.nodes.remove(&place(0, index));
            }
            self.dirty.extend(gone);
            if new > 0 && len % BLOCK as u64 != 0 {
                let mut block = [0; BLOCK];
                read(new - 1, &mut block);
                block[(len % BLOCK as u64) as usize..].fill(0);
                self.set(0, new - 1, leaf(&block));
                self.dirty.push(new - 1);
                self.dirty.sort_unstable();
            }
            self.raise(Digest::height_of(old));
        }
        self.len = len;
        // A taller tree's new levels are over the old top and zeros beside it.
        for level in Digest::height_of(old.min(new)) + 1..=Digest::height_of(new) {
            let (left, right) = (self.get(level - 1, 0), self.get(level - 1, 1));
            self.set(level, 0, node(&left, &right));
        }
    }

    /// The file's bytes in each of `ranges` (offset and length) were written: every block one
    /// touches is read and hashed again, and the nodes above them. `read` is
    /// [`Digest::resize`]'s. A range past the file's end is the caller's bug and panics,
    /// naming it: the length is set first.
    pub fn update(&mut self, ranges: &[(u64, u64)], read: &mut impl FnMut(u64, &mut [u8; BLOCK])) {
        self.dirty.clear();
        for &(at, len) in ranges {
            if len == 0 {
                continue;
            }
            let end = at.checked_add(len).filter(|end| *end <= self.len);
            let Some(end) = end else {
                panic!(
                    "a digest of a file of {:#x} bytes told of a write of {len:#x} bytes at {at:#x}, past its end",
                    self.len
                )
            };
            self.dirty.extend(at / BLOCK as u64..Digest::blocks(end));
        }
        self.dirty.sort_unstable();
        self.dirty.dedup();
        #[cfg(test)]
        {
            if self.plant_skips_a_block {
                self.dirty.pop();
            }
        }
        let mut block = [0; BLOCK];
        for at in 0..self.dirty.len() {
            let index = self.dirty[at];
            block.fill(0);
            read(index, &mut block);
            let end = (index + 1).saturating_mul(BLOCK as u64);
            if end > self.len {
                // What a reader left past the end is not the file's.
                block[BLOCK - (end - self.len) as usize..].fill(0);
            }
            self.set(0, index, leaf(&block));
        }
        let height = self.height();
        self.raise(height);
    }

    /// Hashes again the nodes above the leaves in `dirty`, sorted and each once, up to
    /// `height`: a level's parents are each computed once, however many of their leaves
    /// changed.
    fn raise(&mut self, height: usize) {
        for level in 1..=height {
            if self.dirty.is_empty() {
                return;
            }
            for index in &mut self.dirty {
                *index >>= 1;
            }
            self.dirty.dedup();
            for at in 0..self.dirty.len() {
                let index = self.dirty[at];
                let (left, right) = (self.get(level - 1, 2 * index), self.get(level - 1, 2 * index + 1));
                self.set(level, index, node(&left, &right));
            }
        }
        self.dirty.clear();
    }

    /// The first block where this tree and `other`'s differ, when their roots do: walked from
    /// the top, a level a step. `None` when the two are of one length and content. For an
    /// error that names where a file is not what its link state says.
    pub fn first_difference(&self, other: &Digest) -> Option<u64> {
        if self.len != other.len {
            return Some(Digest::blocks(self.len.min(other.len)).saturating_sub(1));
        }
        let mut level = self.height();
        if self.get(level, 0) == other.get(level, 0) {
            return None;
        }
        let mut index = 0;
        while level > 0 {
            level -= 1;
            index *= 2;
            if self.get(level, index) == other.get(level, index) {
                index += 1;
            }
        }
        Some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    /// One step of a file's life: bytes written at an offset, or a new length.
    #[derive(Clone, Debug)]
    enum Op {
        Write { at: u32, bytes: Vec<u8> },
        Resize(u32),
    }

    #[hegel::composite]
    fn op(tc: &hegel::TestCase) -> Op {
        // Offsets up to 40 blocks, so trees of several levels and files that cross a power of
        // two both ways.
        match tc.draw(gs::integers::<u8>().max_value(3)) {
            0 => Op::Resize(tc.draw(gs::integers::<u32>().max_value(40 * BLOCK as u32))),
            _ => Op::Write {
                at: tc.draw(gs::integers::<u32>().max_value(40 * BLOCK as u32)),
                bytes: tc.draw(gs::binary().max_size(3 * BLOCK)),
            },
        }
    }

    fn reader(file: &[u8]) -> impl FnMut(u64, &mut [u8; BLOCK]) + '_ {
        move |block, out| {
            let from = (block as usize * BLOCK).min(file.len());
            let to = (from + BLOCK).min(file.len());
            out[..to - from].copy_from_slice(&file[from..to]);
        }
    }

    /// `ops` applied to a file and to a digest kept beside it; `Err` naming the first step
    /// after which the kept digest is not the digest of the file hashed whole. `plant` skips a
    /// block of each update.
    fn kept_is_whole(ops: &[Op], plant: bool) -> Result<(), String> {
        let mut file: Vec<u8> = Vec::new();
        let mut kept = Digest::new();
        kept.plant_skips_a_block = plant;
        for (step, op) in ops.iter().enumerate() {
            match op {
                Op::Resize(len) => {
                    file.resize(*len as usize, 0);
                    kept.resize(*len as u64, &mut reader(&file));
                }
                Op::Write { at, bytes } => {
                    let end = *at as usize + bytes.len();
                    if end > file.len() {
                        file.resize(end, 0);
                        kept.resize(end as u64, &mut reader(&file));
                    }
                    file[*at as usize..end].copy_from_slice(bytes);
                    kept.update(&[(u64::from(*at), bytes.len() as u64)], &mut reader(&file));
                }
            }
            let whole = Digest::of(&file);
            if kept.root() != whole.root() {
                return Err(format!(
                    "after step {step} ({op:?}): the kept root differs from the file's, first at block {:?}",
                    kept.first_difference(&whole)
                ));
            }
            if kept.nodes() != whole.nodes() {
                return Err(format!(
                    "after step {step} ({op:?}): {} nodes kept, {} in the file's digest: not canonical",
                    kept.nodes(),
                    whole.nodes()
                ));
            }
        }
        Ok(())
    }

    /// The refinement: after any sequence of writes and changes of length, the digest kept by
    /// updating only the blocks written is the digest of the file hashed whole, root and
    /// nodes.
    #[hegel::test]
    fn a_digest_kept_by_updates_is_the_files(tc: hegel::TestCase) {
        let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(12));
        if let Err(why) = kept_is_whole(&ops, false) {
            panic!("{why}");
        }
    }

    /// A control of [`a_digest_kept_by_updates_is_the_files`]: with an update that skips a
    /// block, a write over two blocks leaves the kept root on the old second block, and the
    /// oracle says at which step.
    #[test]
    fn the_oracle_catches_a_block_not_rehashed() {
        let ops = vec![
            Op::Resize(3 * BLOCK as u32),
            Op::Write { at: BLOCK as u32 - 2, bytes: vec![7; 4] },
        ];
        assert_eq!(kept_is_whole(&ops, false), Ok(()));
        let why = kept_is_whole(&ops, true).expect_err("the oracle missed a block not rehashed");
        assert!(why.contains("after step 1") && why.contains("Some(1)"), "{why}");
    }

    /// The root says the length: a file and the same file with zeros after it differ, though
    /// their blocks pad alike.
    #[test]
    fn the_root_binds_the_length() {
        let (short, long) = (Digest::of(&[1, 2, 3]), Digest::of(&[1, 2, 3, 0]));
        assert_ne!(short.root(), long.root());
        assert_eq!(short.nodes(), long.nodes());
        assert_eq!(Digest::of(&[]).root(), Digest::new().root());
        assert_eq!(Digest::of(&[0; 5 * BLOCK]).nodes(), 0, "a file of zeros keeps no node");
    }

    /// A write of one block of a large file rehashes that block and one node a level: the
    /// reads are counted, and the nodes kept are the path's.
    #[test]
    fn one_block_written_reads_one_block() {
        let mut file = vec![0u8; 1024 * BLOCK];
        let mut kept = Digest::new();
        kept.resize(file.len() as u64, &mut reader(&file));
        file[700 * BLOCK + 9] = 1;
        let mut reads = 0;
        let mut read = |block: u64, out: &mut [u8; BLOCK]| {
            reads += 1;
            out.copy_from_slice(&file[block as usize * BLOCK..][..BLOCK]);
        };
        kept.update(&[(700 * BLOCK as u64 + 9, 1)], &mut read);
        assert_eq!(reads, 1);
        assert_eq!(kept.nodes(), 11, "the leaf and the ten nodes above it");
        assert_eq!(kept.root(), Digest::of(&file).root());
    }
}
