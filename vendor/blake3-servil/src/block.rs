//! The calls' input: [`Block`]s, 64 bytes each on a 64-byte boundary, and
//! [`Batch`], the batch calls' only input, which holds messages of one
//! length in them with every byte past each message zero (BLAKE3 #146,
//! John Servil's proposal; the owning builder, Zooko's choice of October
//! 24; the batch calls taking it, so that a nonzero byte there cannot be
//! written, Zooko's of October 28, BLAKE3 #205).

use crate::BLOCK_LEN;

/// 64 bytes on a 64-byte boundary: the unit the calls read, so every
/// message starts on a cache line. A [`Batch`] holds its messages in
/// blocks, each message starting a block and taking whole blocks (one for
/// an empty message).
#[repr(C, align(64))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Block(pub [u8; BLOCK_LEN]);

impl Block {
    /// A block of zeros.
    pub const ZERO: Block = Block([0; BLOCK_LEN]);

    /// The blocks' bytes, in order.
    pub fn bytes(blocks: &[Block]) -> &[u8] {
        // Sound: a Block is 64 bytes with no padding, and u8 needs no alignment.
        unsafe { core::slice::from_raw_parts(blocks.as_ptr().cast(), blocks.len() * BLOCK_LEN) }
    }

    /// The blocks' bytes, in order, for writing.
    pub fn bytes_mut(blocks: &mut [Block]) -> &mut [u8] {
        // Sound: as in bytes, and the borrow is exclusive.
        unsafe { core::slice::from_raw_parts_mut(blocks.as_mut_ptr().cast(), blocks.len() * BLOCK_LEN) }
    }

    /// `bytes` as blocks, without a copy, when they start on a 64-byte
    /// boundary and are whole blocks (bytes already in place, such as a
    /// mapped file's records), or are empty: `None` otherwise.
    pub fn from_bytes(bytes: &[u8]) -> Option<&[Block]> {
        // An empty slice's pointer says nothing of placement.
        if bytes.is_empty() {
            return Some(&[]);
        }
        if bytes.as_ptr().align_offset(BLOCK_LEN) != 0 || bytes.len() % BLOCK_LEN != 0 {
            return None;
        }
        // Sound: aligned and whole blocks, just checked; Block has no padding.
        Some(unsafe { core::slice::from_raw_parts(bytes.as_ptr().cast(), bytes.len() / BLOCK_LEN) })
    }
}

impl core::fmt::Debug for Block {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Block({:02x?})", &self.0[..])
    }
}

/// Messages of one length in [`Block`]s, the batch calls' input: each
/// message starts a block, and every byte past each message's end is zero,
/// since a `Batch` lets the program write only the messages' own bytes (so
/// the bytes the batch kernels read in place, past a message, can never be
/// wrong). It owns its blocks ([`Batch::new`]), or borrows blocks already in
/// place whose messages fill them, with no bytes past their ends
/// ([`Batch::from_blocks`]: a Merkle layer's 64-byte nodes, a mapped file's
/// records).
///
/// ```
/// # blake3_servil::initialize();
/// # #[cfg(feature = "std")] {
/// use blake3_servil::{Mode, convenience, performance::{self, Batch}};
/// let mut batch = Batch::new(100, 3);
/// for i in 0..3 {
///     batch.set(i, &[i as u8; 100]);
/// }
/// let mut out = [[0u8; 32]; 3];
/// performance::hash_many(Mode::Plain, &batch, &mut out);
/// assert_eq!(out[2], *convenience::hash(&[2u8; 100]).as_bytes());
///
/// // Read straight into a message's own bytes.
/// batch.message_mut(1).copy_from_slice(&[9u8; 100]);
/// performance::hash_many(Mode::Plain, &batch, &mut out);
/// assert_eq!(out[1], *convenience::hash(&[9u8; 100]).as_bytes());
/// # }
/// ```
#[derive(Clone)]
pub struct Batch<'a> {
    #[cfg(feature = "std")]
    blocks: std::borrow::Cow<'a, [Block]>,
    #[cfg(not(feature = "std"))]
    blocks: &'a [Block],
    message_len: usize,
}

/// The blocks each message of `message_len` bytes takes: its bytes in
/// whole blocks, one for an empty message.
#[inline]
fn blocks_per(message_len: usize) -> usize {
    message_len.div_ceil(BLOCK_LEN).max(1)
}

#[cfg(feature = "std")]
impl Batch<'static> {
    /// `count` messages of `message_len` bytes, every byte zero. The zeros
    /// are written now, so all of a `Batch`'s memory is resident from its
    /// making, not as its pages are first touched: make one and refill it.
    #[inline]
    pub fn new(message_len: usize, count: usize) -> Self {
        Batch { blocks: std::vec![Block::ZERO; blocks_per(message_len) * count].into(), message_len }
    }
}

impl<'a> Batch<'a> {
    /// The messages of `message_len` bytes that `blocks` holds, borrowed
    /// with no copy, where they fill their blocks: `message_len` a
    /// multiple of 64 (no byte past a message's end to keep zero; an empty
    /// message's block, which nothing reads, counts as one), and `blocks`
    /// whole messages, which each call that hashes it checks against its
    /// digests by a multiplication: a borrow costs a bit test.
    #[inline]
    pub fn from_blocks(blocks: &'a [Block], message_len: usize) -> Self {
        assert!(
            message_len % BLOCK_LEN == 0,
            "Batch::from_blocks takes messages that fill their blocks, not {message_len} B (Batch::new makes one for any length)"
        );
        Batch { blocks: blocks.into(), message_len }
    }

    /// For the crate's own tests: `blocks` as a batch of `message_len`-byte
    /// messages in place, whatever their length, each at the start of its
    /// blocks and every byte past it zero (checked): the tests' layouts,
    /// batches beside guard pages among them.
    #[cfg(test)]
    pub(crate) fn in_place(blocks: &'a [Block], message_len: usize) -> Self {
        let per = blocks_per(message_len);
        assert!(blocks.len() % per == 0, "{} blocks hold no whole number of {message_len}-byte messages", blocks.len());
        let slot = per * BLOCK_LEN;
        assert!(
            Block::bytes(blocks).chunks_exact(slot).all(|s| message_len == 0 || s[message_len..].iter().all(|&b| b == 0)),
            "a test's batch has a nonzero byte past a message"
        );
        Batch { blocks: blocks.into(), message_len }
    }

    /// The messages' length.
    #[inline]
    pub fn message_len(&self) -> usize {
        self.message_len
    }

    /// How many whole messages it holds.
    #[inline]
    pub fn len(&self) -> usize {
        self.blocks.len() / blocks_per(self.message_len)
    }

    /// Whether it holds no messages.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Its blocks, as the kernels read them: each message at the start of
    /// its blocks, every byte past its end zero.
    #[inline]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }
}

#[cfg(feature = "std")]
impl Batch<'_> {
    /// Message `i`'s bytes, for writing: `message_len` of them, no more (a
    /// borrowed batch copies its blocks first).
    pub fn message_mut(&mut self, i: usize) -> &mut [u8] {
        let per = blocks_per(self.message_len);
        assert!((i + 1) * per <= self.blocks.len(), "message {i} of a batch of {}", self.len());
        let len = self.message_len;
        &mut Block::bytes_mut(&mut self.blocks.to_mut()[i * per..][..per])[..len]
    }

    /// Message `i` becomes `message`, `message_len` bytes.
    pub fn set(&mut self, i: usize, message: &[u8]) {
        self.message_mut(i).copy_from_slice(message);
    }

    /// Every byte for writing, where no message has bytes past its end to
    /// keep zero: `message_len` a multiple of 64 and not 0.
    pub fn blocks_mut(&mut self) -> &mut [Block] {
        assert!(self.message_len % BLOCK_LEN == 0 && self.message_len > 0, "a batch whose messages leave no bytes past their ends ({} B)", self.message_len);
        self.blocks.to_mut()
    }

    /// Its 64-byte messages as 32-byte digests, two a message: a Merkle
    /// layer's digests written straight into the next layer, whose
    /// messages are pairs of children. A layer of an odd count leaves a
    /// digest to spare: slice it, `&mut batch.digests_mut()[..count]`.
    pub fn digests_mut(&mut self) -> &mut [[u8; 32]] {
        assert!(self.message_len == 2 * 32, "digests_mut takes a batch of 64-byte messages, two digests each, not {} B", self.message_len);
        let blocks = self.blocks.to_mut();
        // Sound: a [u8; 32] is 32 bytes with alignment 1; two fill a Block.
        unsafe { core::slice::from_raw_parts_mut(blocks.as_mut_ptr().cast(), blocks.len() * 2) }
    }
}

#[cfg(all(test, feature = "std"))]
mod test {
    use super::*;
    use crate::Mode;
    use crate::performance::hash_many;

    /// Every byte a Batch lets the program write, written nonzero: every
    /// byte past each message's end stays zero, and each digest is the
    /// message's own (convenience::hash, an independent path).
    #[test]
    fn test_a_batch_keeps_the_bytes_past_its_messages_zero() {
        crate::initialize();
        for len in [0, 1, 63, 64, 65, 100, 1000, 1024, 1025] {
            let mut batch = Batch::new(len, 5);
            for round in 1..=2u8 {
                for i in 0..batch.len() {
                    batch.message_mut(i).fill(round * 16 + i as u8);
                }
                let per = len.div_ceil(BLOCK_LEN).max(1) * BLOCK_LEN;
                for (i, slot) in Block::bytes(batch.blocks()).chunks(per).enumerate() {
                    assert!(slot[len..].iter().all(|&b| b == 0), "{len} B, message {i}: its padding");
                }
                let mut out = [[0u8; 32]; 5];
                hash_many(Mode::Plain, &batch, &mut out);
                for (i, d) in out.iter().enumerate() {
                    assert_eq!(*d, *crate::convenience::hash(&std::vec![round * 16 + i as u8; len]).as_bytes(), "{len} B, message {i}, round {round}");
                }
            }
        }
    }

    #[test]
    fn test_from_bytes_takes_aligned_whole_blocks_only() {
        let blocks = [Block::ZERO; 3];
        let bytes = Block::bytes(&blocks);
        assert_eq!(Block::from_bytes(bytes).map(<[Block]>::len), Some(3));
        assert_eq!(Block::from_bytes(&bytes[..128]).map(<[Block]>::len), Some(2));
        assert!(Block::from_bytes(&bytes[16..80]).is_none(), "16 bytes into a line");
        assert!(Block::from_bytes(&bytes[..100]).is_none(), "a partial block");
        assert_eq!(Block::from_bytes(&[]).map(<[Block]>::len), Some(0));
    }

    /// Blocks already in place, borrowed with no copy where the messages
    /// fill them; a length that leaves bytes past a message is refused (only
    /// Batch::new makes one), and blocks of no whole message count are
    /// refused when hashed, against the digests.
    #[test]
    fn test_from_blocks_borrows_messages_that_fill_their_blocks() {
        crate::initialize();
        let blocks: std::vec::Vec<Block> = (0..4).map(|i| Block([i as u8; 64])).collect();
        let batch = Batch::from_blocks(&blocks, 128);
        assert_eq!((batch.len(), batch.blocks().as_ptr()), (2, blocks.as_ptr()), "borrowed, not copied");
        let mut out = [[0u8; 32]; 2];
        hash_many(Mode::Plain, &batch, &mut out);
        assert_eq!(out[1], *crate::convenience::hash(Block::bytes(&blocks[2..])).as_bytes());
        assert!(std::panic::catch_unwind(|| Batch::from_blocks(&blocks, 100)).is_err(), "100-byte messages");
        let partial = Batch::from_blocks(&blocks[..3], 128);
        assert!(std::panic::catch_unwind(move || hash_many(Mode::Plain, &partial, &mut [[0u8; 32]; 1])).is_err(), "three blocks of 128-byte messages");
    }

    #[test]
    #[should_panic(expected = "digests_mut takes a batch of 64-byte messages")]
    fn test_digests_mut_takes_64_byte_messages() {
        Batch::new(128, 2).digests_mut();
    }

    #[test]
    #[should_panic(expected = "leave no bytes past their ends")]
    fn test_blocks_mut_refuses_messages_with_padding() {
        Batch::new(100, 2).blocks_mut();
    }

    /// A Merkle layer: leaves' digests written by hash_many straight into
    /// the parents' batch, two a block, then the parents hashed: the root
    /// of four leaves is a parent of parents, keyed alike (as a tree's
    /// caller keys its nodes).
    #[test]
    fn test_a_merkle_layer_lands_in_the_next_layers_blocks() {
        crate::initialize();
        let key = [5u8; 32];
        let mut leaves = Batch::new(256, 4);
        (0..4).for_each(|i| leaves.set(i, &[i as u8; 256]));
        let mut parents = Batch::new(64, 2);
        hash_many(Mode::Keyed(&key), &leaves, parents.digests_mut());
        let mut root = [[0u8; 32]; 2];
        hash_many(Mode::Keyed(&key), &parents, &mut root);
        let leaf = |i: u8| *crate::convenience::hash_with(Mode::Keyed(&key), &[i; 256]).as_bytes();
        let parent = |a: [u8; 32], b: [u8; 32]| *crate::convenience::hash_with(Mode::Keyed(&key), &[a, b].concat()).as_bytes();
        assert_eq!(root[0], parent(leaf(0), leaf(1)));
        assert_eq!(root[1], parent(leaf(2), leaf(3)));
    }

    /// What a Batch says of itself: its message length and count, empty at
    /// zero messages; two digests a 64-byte message from digests_mut, each landing on
    /// its own 32 bytes; a Block's Debug form names its bytes.
    #[test]
    fn test_a_batch_describes_itself() {
        let batch = Batch::new(100, 3);
        assert_eq!((batch.message_len(), batch.len(), batch.is_empty()), (100, 3, false));
        assert_eq!(batch.blocks().len(), 6, "two blocks a 100-byte message");
        let none = Batch::new(100, 0);
        assert_eq!((none.len(), none.is_empty()), (0, true));
        let mut parents = Batch::new(64, 3);
        let digests = parents.digests_mut();
        assert_eq!(digests.len(), 6);
        digests[5] = [7; 32];
        assert_eq!(&parents.blocks()[2].0[32..], &[7u8; 32][..], "the sixth digest is block 2's second half");
        assert_eq!(&parents.blocks()[2].0[..32], &[0u8; 32][..]);
        assert!(std::format!("{:?}", Block::ZERO).starts_with("Block([00, 00"));
    }
}
