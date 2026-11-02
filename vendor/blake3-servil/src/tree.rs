//! The single-thread tree walk, in safe Rust over its kernels.

use crate::platform::{MAX_SIMD_DEGREE, Platform};
use crate::{CHUNK_LEN, CVWords, OUT_LEN};

/// A chaining value.
pub(crate) type Cv = [u8; OUT_LEN];

/// The most values a walk returns: the largest degree, at least 2.
const MAX: usize = MAX_SIMD_DEGREE;

/// The kernels the walk calls.
pub trait Kernels {
    /// The chunks hashed per call: a power of two, 2 to MAX.
    fn degree(&self) -> usize;
    /// The chaining values of the chunks of `input` (1 to degree() chunks:
    /// whole ones, then perhaps a shorter last one), the first numbered
    /// `counter`, into `out[..n]`; returns n, the number of chunks.
    fn chunks(&self, input: &[u8], counter: u64, out: &mut [Cv]) -> usize;
    /// The parents of the pairs of `children` (2 to 2 MAX of them) into
    /// `out`, an odd last child copied after them; returns their count.
    fn parents(&self, children: &[Cv], out: &mut [Cv]) -> usize;
    /// What `wide` returns for `input`, computed another way where the
    /// kernels have one (None: they have none for it). `ahead` bytes of the
    /// caller's input follow `input`.
    fn subtree(&self, input: &[u8], ahead: usize, counter: u64, out: &mut [Cv]) -> Option<usize>;
    /// What `root_children` returns for `input`, computed another way where
    /// the kernels have one (None: they have none for it).
    fn children(&self, input: &[u8], ahead: usize, counter: u64) -> Option<[Cv; 2]>;
    /// The root output (32 bytes) of an input of at most CHUNK_LEN bytes:
    /// its only chunk, numbered 0, as the root.
    fn chunk_root(&self, input: &[u8]) -> Cv;
    /// The root output (32 bytes) of the parent of `children`, as the root.
    fn parent_root(&self, children: &[Cv; 2]) -> Cv;
}

/// The bytes of the left subtree of an input of `len` > CHUNK_LEN bytes:
/// the largest power of two chunks below its chunk count.
pub fn left_len(len: usize) -> usize {
    let chunks = (len - 1) / CHUNK_LEN;
    // The largest power of two at most `chunks` (1 or more), from its top
    // 1-bit, as each.rs's left_len: no doubling loop that a change could
    // keep from ending.
    (1usize << (usize::BITS - 1 - chunks.leading_zeros())) * CHUNK_LEN
}

/// The chaining values of a cut through the subtree over `input` (at least
/// one byte), its first chunk numbered `counter`, into `out[..n]`; returns
/// n, between 1 and degree. `out` holds at least that many.
/// `ahead` bytes of the caller's input follow `input`.
pub fn wide<K: Kernels>(k: &K, input: &[u8], ahead: usize, counter: u64, out: &mut [Cv]) -> usize {
    if input.len() <= k.degree() * CHUNK_LEN {
        return k.chunks(input, counter, out);
    }
    if let Some(n) = k.subtree(input, ahead, counter, out) {
        return n;
    }
    let l = left_len(input.len());
    let (a, b) = input.split_at(l);
    // More than degree chunks (2 or more), so the left half holds two
    // chunks or more, and gives two values or more (up to degree).
    let mut cvs = [[0u8; OUT_LEN]; 2 * MAX];
    let (lo, hi) = cvs.split_at_mut(k.degree());
    let ln = wide(k, a, b.len() + ahead, counter, lo);
    let rn = wide(k, b, ahead, counter + (l / CHUNK_LEN) as u64, hi);
    k.parents(&cvs[..ln + rn], out)
}

/// The chaining values of the root's two children, for `input` of more
/// than one chunk, its first chunk numbered `counter`: the walk's values,
/// condensed a layer of parents at a time. `cvs` holds at least
/// degree values and `next` half that. `ahead` bytes of the
/// caller's input follow `input`.
pub fn children<K: Kernels>(k: &K, input: &[u8], ahead: usize, counter: u64, cvs: &mut [Cv], next: &mut [Cv]) -> [Cv; 2] {
    let mut n = wide(k, input, ahead, counter, cvs);
    while n > 2 {
        n = k.parents(&cvs[..n], next);
        cvs[..n].copy_from_slice(&next[..n]);
    }
    [cvs[0], cvs[1]]
}

/// Inputs of at most this many chunks condense in arrays of this size.
pub const SMALL: usize = 16;

/// The chaining values of the root's two children, for `input` of more
/// than one chunk, its first chunk numbered `counter`; `ahead` bytes of the
/// caller's input follow it. Out of line: its arrays (6 KiB at a degree of
/// 128) stay out of its callers' frames, and an input of SMALL chunks or
/// fewer gets arrays of that size.
#[inline(never)]
pub fn root_children<K: Kernels>(k: &K, input: &[u8], ahead: usize, counter: u64) -> [Cv; 2] {
    if let Some(c) = k.children(input, ahead, counter) {
        return c;
    }
    if input.len() <= SMALL * CHUNK_LEN {
        let mut cvs = [[0u8; OUT_LEN]; SMALL];
        let mut next = [[0u8; OUT_LEN]; SMALL / 2];
        children(k, input, ahead, counter, &mut cvs, &mut next)
    } else {
        let mut cvs = [[0u8; OUT_LEN]; MAX];
        let mut next = [[0u8; OUT_LEN]; MAX / 2];
        children(k, input, ahead, counter, &mut cvs, &mut next)
    }
}

/// The hash (the 32-byte root output) of `input`.
#[inline(always)]
pub fn hash<K: Kernels>(k: &K, input: &[u8]) -> Cv {
    if input.len() <= CHUNK_LEN {
        return k.chunk_root(input);
    }
    hash_tree(k, input)
}

/// A tree of two chunks or more, out of line: its frame (the children's
/// chaining values, 64-byte aligned, and the registers it saves) would
/// otherwise be set up on every call, a chunk or less included, whose one
/// kernel call needs none of it.
#[inline(never)]
fn hash_tree<K: Kernels>(k: &K, input: &[u8]) -> Cv {
    k.parent_root(&root_children(k, input, 0, 0))
}

/// The library's kernels for one key and flags on one platform.
pub(crate) struct PlatformKernels<'a> {
    pub key: &'a CVWords,
    pub flags: u8,
    pub platform: Platform,
}

impl Kernels for PlatformKernels<'_> {
    #[inline(always)]
    fn degree(&self) -> usize {
        self.platform.simd_degree()
    }

    #[inline(always)]
    fn chunks(&self, input: &[u8], counter: u64, out: &mut [Cv]) -> usize {
        crate::compress_chunks_parallel(input, self.key, counter, self.flags, self.platform, out.as_flattened_mut())
    }

    #[inline(always)]
    fn parents(&self, children: &[Cv], out: &mut [Cv]) -> usize {
        crate::compress_parents_parallel(children.as_flattened(), self.key, self.flags, self.platform, out.as_flattened_mut())
    }

    #[inline(always)]
    #[allow(unused_variables)]
    fn subtree(&self, input: &[u8], ahead: usize, counter: u64, out: &mut [Cv]) -> Option<usize> {
        // Whole subtrees of 32 KiB to 1 MiB on SME2: the flat walk, SME2
        // alone (see sme2::compress_subtree_flat).
        #[cfg(blake3_sme2)]
        if matches!(self.platform, Platform::SME2) && crate::sme2::flat_takes(input.len()) {
            // Safe: the SME2 platform is selected only where the CPU has it,
            // and `out` holds simd_degree() values, which is sme2::DEGREE.
            return Some(unsafe { crate::sme2::compress_subtree_flat(input, ahead, self.key, counter, self.flags, out.as_flattened_mut()) });
        }
        None
    }

    #[inline(always)]
    #[allow(unused_variables)]
    fn children(&self, input: &[u8], ahead: usize, counter: u64) -> Option<[Cv; 2]> {
        // Whole subtrees of 32 KiB to 1 MiB on SME2: the flat walk down to
        // the two children, SME2 alone (see sme2::compress_subtree_flat_to_parent).
        #[cfg(blake3_sme2)]
        if matches!(self.platform, Platform::SME2) && crate::sme2::flat_takes(input.len()) {
            // Safe: the SME2 platform is selected only where the CPU has it.
            let block = unsafe { crate::sme2::compress_subtree_flat_to_parent(input, ahead, self.key, counter, self.flags) };
            return Some([block[..OUT_LEN].try_into().unwrap(), block[OUT_LEN..].try_into().unwrap()]);
        }
        None
    }

    #[inline(always)]
    fn chunk_root(&self, input: &[u8]) -> Cv {
        // The scalar kernel, on every AArch64 core (crate::hash_one_chunk_root).
        #[cfg(blake3_neon)]
        return crate::hash_one_chunk_root(input, self.key, self.flags).0;
        // One kernel call that keeps off the stack (src/ffi_chunk_root.rs):
        // a caller's just-written message has none of our loads to hold back.
        // Sound: detect() found the platform's instructions.
        #[cfg(target_arch = "x86_64")]
        match self.platform {
            crate::platform::Platform::AVX512 => unsafe { crate::chunk_root::chunk_root(true, input, self.key, self.flags) },
            crate::platform::Platform::SSE41 | crate::platform::Platform::AVX2 => unsafe { crate::chunk_root::chunk_root(false, input, self.key, self.flags) },
        }
    }

    #[inline(always)]
    fn parent_root(&self, children: &[Cv; 2]) -> Cv {
        crate::hasher::parent_output(&children[0], &children[1], self.key, self.flags, self.platform).root_bytes()
    }
}
