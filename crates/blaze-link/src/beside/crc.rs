//! The CRC-32 of a side file kept through its patches: a tree of sums over 4 KiB blocks, so
//! a patch that writes a few ranges reads those blocks and a path to the root a block, never
//! the file.
//!
//! Until 2026-11-11 every update summed the whole file again to seal it (8.6 MB of
//! hello world's, 19.5 ms of a one-function edit's link, `Kept::seal`): the one cost of the
//! side file that was the program's and not the edit's.
//!
//! Why a tree works. Feeding a byte to the CRC register is linear over GF(2) in the register
//! and the byte, so with `F(r, M)` the register after feeding `M` from `r`:
//! `F(r, M) = F(r, 0^|M|) ^ F(0, M)` and `F(0, A ++ B) = F(F(0, A), 0^|B|) ^ F(0, B)`.
//! `F(0, M)` is a node's sum; `Z_n(r) = F(r, 0^n)` (feeding `n` zero bytes) is a linear map,
//! a 32 by 32 matrix a power of two of bytes ([`POW`]). Zeros in front change nothing
//! (`F(0, 0^n ++ M) = F(0, M)`), so the bytes sit at the end of the leaves and the tree's
//! shape is of the length alone. The CRC-32 is `!(Z_len(!0) ^ root)`.

use super::CRC;

/// The bytes a leaf sums.
const BLOCK: usize = 4096;
const BLOCK_LOG: usize = 12;

/// `POW[j]` is `Z` of `2^j` zero bytes, as its image of each register bit.
static POW: [[u32; 32]; 48] = pow();

const fn times(matrix: &[u32; 32], mut reg: u32) -> u32 {
    let mut out = 0;
    let mut bit = 0;
    while reg != 0 {
        if reg & 1 != 0 {
            out ^= matrix[bit];
        }
        reg >>= 1;
        bit += 1;
    }
    out
}

const fn pow() -> [[u32; 32]; 48] {
    let table = super::crc_table();
    let mut out = [[0u32; 32]; 48];
    let mut bit = 0;
    while bit < 32 {
        let reg = 1u32 << bit;
        out[0][bit] = table[(reg & 0xff) as usize] ^ (reg >> 8);
        bit += 1;
    }
    let mut j = 1;
    while j < 48 {
        let mut bit = 0;
        while bit < 32 {
            out[j][bit] = times(&out[j - 1], out[j - 1][bit]);
            bit += 1;
        }
        j += 1;
    }
    out
}

/// The register `reg` after `count` zero bytes.
fn zeros(mut reg: u32, count: u64) -> u32 {
    assert!(count < 1 << 48, "a file of 2^48 bytes or more");
    let mut j = 0;
    let mut left = count;
    while left != 0 {
        if left & 1 != 0 {
            reg = times(&POW[j], reg);
        }
        left >>= 1;
        j += 1;
    }
    reg
}

/// `F(0, bytes)`.
fn sum(bytes: &[u8]) -> u32 {
    let mut reg = 0u32;
    for byte in bytes {
        reg = CRC[((reg ^ u32::from(*byte)) & 0xff) as usize] ^ (reg >> 8);
    }
    reg
}

/// The sums of a file's blocks ([`self`]).
pub(super) struct Sums {
    /// The file's length.
    len: usize,
    /// A power of two; the bytes are the last `len` of `leaves * BLOCK`.
    leaves: usize,
    /// A heap: the root at 1, the leaves from `leaves`.
    nodes: Vec<u32>,
    /// The bytes read to make and to keep the sums: what a test holds to the patch.
    pub(super) read: u64,
}

impl Sums {
    /// The sums of `bytes`, every block read.
    pub(super) fn of(bytes: &[u8]) -> Sums {
        let leaves = bytes.len().div_ceil(BLOCK).max(1).next_power_of_two();
        // alloc: once per side file laid out whole, 4 bytes a block
        let mut sums = Sums { len: bytes.len(), leaves, nodes: vec![0; 2 * leaves], read: 0 };
        for leaf in 0..leaves {
            sums.nodes[leaves + leaf] = sums.leaf(bytes, leaf);
        }
        for node in (1..leaves).rev() {
            sums.nodes[node] = sums.join(node);
        }
        sums
    }

    /// The bytes the sums hold.
    pub(super) fn held(&self) -> usize {
        self.nodes.capacity() * size_of::<u32>()
    }

    /// The length of the file these are the sums of.
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The range of the file leaf `leaf` sums: empty for one wholly in the zeros in front.
    fn range(&self, leaf: usize) -> std::ops::Range<usize> {
        let pad = self.leaves * BLOCK - self.len;
        (leaf * BLOCK).saturating_sub(pad)..((leaf + 1) * BLOCK).saturating_sub(pad)
    }

    fn leaf(&mut self, bytes: &[u8], leaf: usize) -> u32 {
        let range = self.range(leaf);
        self.read += range.len() as u64;
        sum(&bytes[range])
    }

    /// The sum of `node`'s two halves: the left's after the right's zeros, and the right's.
    fn join(&self, node: usize) -> u32 {
        // A node at `height` above the leaves covers `2^height` blocks, its right half
        // `2^(height - 1)`.
        let height = (self.leaves.ilog2() - node.ilog2()) as usize;
        times(&POW[BLOCK_LOG + height - 1], self.nodes[2 * node]) ^ self.nodes[2 * node + 1]
    }

    /// Takes a write of `bytes[at..at + len]`: `bytes` is the file after it, of the length
    /// the sums were made of. The blocks the range touches are read again, and each one's
    /// path to the root joined again.
    #[cfg(test)]
    pub(super) fn wrote(&mut self, bytes: &[u8], at: u64, len: u64) {
        self.wrote_all(bytes, &[(at, len)]);
    }

    /// Takes the writes of `bytes[at..at + len]` for each of `ranges`, in any order and
    /// overlapping or not: `bytes` is the file after them, of the length the sums were made
    /// of. Each block a range touches is read again once, however many ranges touch it, and
    /// its path to the root joined again.
    pub(super) fn wrote_all(&mut self, bytes: &[u8], ranges: &[(u64, u64)]) {
        assert_eq!(bytes.len(), self.len, "a file of another length is summed whole");
        let pad = self.leaves * BLOCK - self.len;
        // alloc: once per patch sealed, the blocks its writes touch: a few
        let mut touched: Vec<usize> = Vec::new();
        for &(at, len) in ranges {
            let end = (at + len).min(self.len as u64) as usize;
            let at = at as usize;
            if at < end {
                touched.extend((at + pad) / BLOCK..=(end - 1 + pad) / BLOCK);
            }
        }
        touched.sort_unstable();
        touched.dedup();
        for leaf in touched {
            let mut node = self.leaves + leaf;
            self.nodes[node] = self.leaf(bytes, leaf);
            while node > 1 {
                node /= 2;
                self.nodes[node] = self.join(node);
            }
        }
    }

    /// The file's CRC-32.
    pub(super) fn crc32(&self) -> u32 {
        !(zeros(!0, self.len as u64) ^ self.nodes[1])
    }
}

#[cfg(test)]
mod tests {
    use blaze_sim::Config;

    use super::super::crc32;
    use super::*;

    fn file(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// The refinement: after any sequence of writes, the sums kept through them give the
    /// CRC-32 a pass over the whole file gives, for files of any length (none, less than a
    /// block, a block's multiple, between).
    #[test]
    fn sums_kept_through_writes_give_the_whole_files_crc() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            let len: usize = tape.int_in(0..=5 * BLOCK + 17, "len");
            let mut bytes = file(tape.int::<u64>("file seed"), len);
            let mut sums = Sums::of(&bytes);
            assert_eq!(sums.crc32(), crc32(&bytes), "made whole, {len} bytes");
            let writes: usize = tape.int_in(0..=12usize, "writes");
            for _ in 0..writes {
                let at: usize = tape.int_in(0..=len + 8, "at");
                let count: usize = tape.int_in(0..=2 * BLOCK, "count");
                let value: u8 = tape.int::<u8>("value");
                let end = (at + count).min(len);
                for byte in bytes.get_mut(at.min(len)..end).unwrap_or(&mut []) {
                    *byte = byte.wrapping_add(value);
                }
                sums.wrote(&bytes, at as u64, count as u64);
                assert_eq!(sums.crc32(), crc32(&bytes), "after a write of {count} at {at} of {len}");
            }
        })
    }

    /// The control of [`sums_kept_through_writes_give_the_whole_files_crc`]: a write the
    /// sums are not told of (the planted bug) leaves a CRC that is not the file's.
    #[test]
    fn the_oracle_catches_a_write_the_sums_were_not_told_of() {
        let mut bytes = file(7, 3 * BLOCK + 100);
        let mut sums = Sums::of(&bytes);
        bytes[BLOCK + 5] ^= 1;
        assert_ne!(sums.crc32(), crc32(&bytes), "the sums are of the file before the write");
        sums.wrote(&bytes, (BLOCK + 5) as u64, 1);
        assert_eq!(sums.crc32(), crc32(&bytes));
    }

    /// What a write costs is the write's, not the file's: the same bytes read for the same
    /// write in a file of 64 KiB and one of 16 MiB. The control: sums made whole read every
    /// byte, which the two sizes tell apart.
    #[test]
    fn a_write_reads_its_blocks_whatever_the_files_length() {
        let read_of = |len: usize| {
            let mut bytes = file(3, len);
            let mut sums = Sums::of(&bytes);
            assert_eq!(sums.read, len as u64, "made whole: every byte once");
            let before = sums.read;
            bytes[len - 9000] ^= 0x55;
            bytes[len - 10] ^= 0x55;
            sums.wrote(&bytes, (len - 9000) as u64, 1);
            sums.wrote(&bytes, (len - 10) as u64, 4);
            assert_eq!(sums.crc32(), crc32(&bytes));
            sums.read - before
        };
        let (small, large) = (read_of(64 << 10), read_of(16 << 20));
        assert_eq!(small, large, "two blocks read, in a file of any length");
        assert_eq!(small, 2 * BLOCK as u64);
    }

    /// Ranges that touch one block read it once: ten writes in one block and one in another
    /// are two blocks read, as the writes of a patch are (a record, its name, its rows).
    #[test]
    fn writes_in_one_block_read_it_once() {
        let mut bytes = file(5, 20 * BLOCK);
        let mut sums = Sums::of(&bytes);
        let before = sums.read;
        let mut ranges: Vec<(u64, u64)> = (0..10u64).map(|n| (3 * BLOCK as u64 + 100 * n, 8)).collect();
        ranges.push((9 * BLOCK as u64 + 7, 3));
        for &(at, len) in &ranges {
            for byte in &mut bytes[at as usize..(at + len) as usize] {
                *byte ^= 0x3c;
            }
        }
        sums.wrote_all(&bytes, &ranges);
        assert_eq!(sums.crc32(), crc32(&bytes));
        assert_eq!(sums.read - before, 2 * BLOCK as u64);
    }
}
