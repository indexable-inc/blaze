//! The Hasher: a message fed in pieces (a power user's call; see
//! crate::performance, which exports it).

use super::*;

/// A message's hash, fed in pieces: [`update`](Hasher::update) with each
/// piece, in order, then [`finalize`](Hasher::finalize). The pieces may be
/// of any lengths; the hash is the same as [`hash`](convenience::hash) of them all at once.
///
/// A `Hasher` is about 18 KiB (it holds short pieces until it has 16 KiB
/// to hash at once); box it where that matters.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # blake3_servil::initialize();
/// // Hash an input incrementally.
/// let mut hasher = blake3_servil::performance::Hasher::new(blake3_servil::Mode::Plain);
/// hasher.update(b"foo");
/// hasher.update(b"bar");
/// hasher.update(b"baz");
/// assert_eq!(hasher.finalize(), blake3_servil::convenience::hash(b"foobarbaz"));
///
/// // Extended output. OutputReader also implements Read and Seek.
/// # #[cfg(feature = "std")] {
/// let mut output = [0; 1000];
/// let mut output_reader = hasher.finalize_xof();
/// output_reader.fill(&mut output);
/// assert_eq!(&output[..32], blake3_servil::convenience::hash(b"foobarbaz").as_bytes());
/// # }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Hasher {
    pub(crate) core: HasherCore,
    /// The message's bytes from its last 16 KiB boundary, gathered while
    /// they arrive in shorter updates: the first `staged`.
    pub(crate) staged: usize,
    pub(crate) stage: [core::mem::MaybeUninit<u8>; STAGE_LEN],
}

/// The bytes a Hasher gathers before they hash side by side: 16 chunks,
/// the widest kernels' lanes.
pub(crate) const STAGE_LEN: usize = 16 * CHUNK_LEN;

/// The tree's state behind a [`Hasher`]: its chunk, and the chaining values
/// of its finished subtrees.
#[derive(Clone)]
pub(crate) struct HasherCore {
    pub(crate) key: CVWords,
    pub(crate) chunk_state: ChunkState,
    pub(crate) initial_chunk_counter: u64,
    // The stack size is MAX_DEPTH + 1 because we do lazy merging. For example,
    // with 7 chunks, we have 3 entries in the stack. Adding an 8th chunk
    // requires a 4th entry, rather than merging everything down to 1, because
    // we don't know whether more input is coming (official BLAKE3's
    // reference implementation merges eagerly instead).
    pub(crate) cv_stack: CvStack,
}

/// What the Hasher's kernels take from the library (hasher::Hooks): with
/// `pooled`, subtrees of lanes::MIN_SPLIT_LEN and more on the pool; whole
/// subtrees on `platform` (an SME2 turn's); `chunk_platform`, the chunk
/// state's, otherwise.
struct HasherHooks<'a> {
    pub(crate) key: &'a CVWords,
    flags: u8,
    chunk_platform: Platform,
    platform: Platform,
    pooled: bool,
}

impl hasher::Hooks for HasherHooks<'_> {
    #[inline(always)]
    #[allow(unused_variables)]
    fn subtree_cv(&self, input: &[u8], ahead: usize, counter: u64) -> Option<CVBytes> {
        // After the first input, no subtree is the root: on SME2 the flat
        // walk runs it down to its own chaining value, so one value goes
        // on the stack and about one merge runs on the core between SME2
        // kernels (about 0.2 us of other work between them puts the SME
        // unit in its slow state: `sme2`'s documentation).
        #[cfg(blake3_sme2)]
        if !self.pooled && matches!(self.platform, Platform::SME2) && sme2::flat_takes(input.len()) {
            // Safe: the SME2 platform is selected only where the CPU has it.
            return Some(unsafe { sme2::compress_subtree_flat_to_cv(input, ahead, self.key, counter, self.flags) });
        }
        None
    }

    #[inline(always)]
    fn children(&self, input: &[u8], ahead: usize, counter: u64) -> [CVBytes; 2] {
        #[cfg(feature = "std")]
        let pool_takes = self.pooled && input.len() >= lanes::MIN_SPLIT_LEN;
        #[cfg(not(feature = "std"))]
        let pool_takes = false;
        let pair = if pool_takes {
            #[cfg(feature = "std")]
            {
                lanes::subtree_children(input, self.key, counter, self.flags)
            }
            #[cfg(not(feature = "std"))]
            unreachable!()
        } else if self.pooled {
            // Pooled, the subtrees below take the lock one at a time, and
            // the pool's own never use SME2.
            let own = platform::Sme2Turn::take(self.chunk_platform, input.len() >= SME2_SIZED_LEN);
            compress_subtree_to_parent_node(input, ahead, self.key, counter, self.flags, own.platform())
        } else {
            compress_subtree_to_parent_node(input, ahead, self.key, counter, self.flags, self.platform)
        };
        [pair[..OUT_LEN].try_into().unwrap(), pair[OUT_LEN..].try_into().unwrap()]
    }
}

/// The Hasher's kernels: chunks, nodes, and parents on the chunk state's
/// platform, the rest from `hooks`.
type HasherKernels<'a> = hasher::Kernels<'a, Platform, HasherHooks<'a>>;

/// A Hasher's kernels on its key and chunk state (see HasherHooks).
#[inline(always)]
fn kernels_on<'a>(key: &'a CVWords, chunk_state: &ChunkState, platform: Platform, pooled: bool) -> HasherKernels<'a> {
    hasher::Kernels {
        key,
        flags: chunk_state.flags,
        platform: chunk_state.platform,
        hooks: HasherHooks { key, flags: chunk_state.flags, chunk_platform: chunk_state.platform, platform, pooled },
    }
}

impl Hasher {
    pub(crate) fn from_core(core: HasherCore) -> Self {
        Self { core, staged: 0, stage: [core::mem::MaybeUninit::uninit(); STAGE_LEN] }
    }

    /// The bytes gathered in the stage.
    fn staged_bytes(&self) -> &[u8] {
        // Sound: the first `staged` bytes of the stage were written (gather).
        unsafe { core::slice::from_raw_parts(self.stage.as_ptr() as *const u8, self.staged) }
    }

    /// Copy `bytes` into the stage after what it holds.
    pub(crate) fn stage(&mut self, bytes: &[u8]) {
        // Sound: MaybeUninit<u8> has u8's layout, and the bytes fit (gather).
        let room = unsafe { core::slice::from_raw_parts_mut(self.stage.as_mut_ptr().add(self.staged) as *mut u8, bytes.len()) };
        room.copy_from_slice(bytes);
        self.staged += bytes.len();
    }

    /// `update`'s gathering: an update of STAGE_LEN or more from a 16 KiB
    /// boundary of the message (or an empty stage) goes to the core whole;
    /// shorter ones gather in the stage until it reaches the next boundary
    /// (but a fresh hasher's first chunk goes to the core),
    /// and the stage goes to the core as one update, its chunks side by
    /// side in the lanes.
    fn gather(&mut self, input: &[u8], pooled: bool) -> &mut Self {
        self.gather_part(input, pooled, false);
        self
    }

    /// [`gather`](Hasher::gather), but with `hold`, a stage that fills a
    /// whole group (from offset zero) is kept for the caller to hash with
    /// others' (`held_group`), and the rest of `input`
    /// returned; else nothing is left.
    pub(crate) fn gather_part<'a>(&mut self, mut input: &'a [u8], pooled: bool, hold: bool) -> &'a [u8] {
        self.core.assert_room(self.count(), input.len());
        while !input.is_empty() {
            // Whole groups, and a fresh hasher's first chunk (which hashes
            // serially either way), skip the stage.
            if self.staged == 0 && (input.len() >= STAGE_LEN || self.core.count() + input.len() as u64 <= CHUNK_LEN as u64) {
                self.core.update_pooled(input, pooled);
                return &[];
            }
            let at = (self.core.initial_chunk_counter * CHUNK_LEN as u64 + self.count()) % STAGE_LEN as u64;
            let to_boundary = STAGE_LEN - at as usize;
            let take = cmp::min(to_boundary, input.len());
            // The loop's variant: every pass stages at least one byte of
            // the input, which is not empty (`at` is below STAGE_LEN).
            assert!(take > 0, "the stage took nothing");
            self.stage(&input[..take]);
            input = &input[take..];
            if take < to_boundary {
                return &[];
            }
            if hold && self.held_group().is_some() {
                return input;
            }
            let staged = core::mem::replace(&mut self.staged, 0);
            // Sound: the first `staged` bytes were written; the core reads
            // them before the stage is written again.
            let bytes = unsafe { core::slice::from_raw_parts(self.stage.as_ptr() as *const u8, staged) };
            self.core.update_pooled(bytes, pooled);
        }
        &[]
    }

    /// A whole group in the stage, from offset zero: its first chunk's
    /// counter and its bytes.
    #[cfg(feature = "std")]
    pub(crate) fn held_group(&self) -> Option<(u64, &[u8; STAGE_LEN])> {
        (self.staged == STAGE_LEN && self.core.initial_chunk_counter == 0)
            .then(|| (self.core.count() / CHUNK_LEN as u64, self.staged_bytes().try_into().unwrap()))
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn held_group(&self) -> Option<(u64, &[u8; STAGE_LEN])> {
        None
    }

    /// The held group's subtree, as [`HasherCore::update_with_results`]
    /// takes it (its two halves' values at chunk zero, else its own), into
    /// the core; the stage empty again.
    #[cfg(feature = "std")]
    pub(crate) fn take_held(&mut self, result: &[u8; BLOCK_LEN]) {
        let staged = core::mem::replace(&mut self.staged, 0);
        // Sound: as in gather_part.
        let bytes = unsafe { core::slice::from_raw_parts(self.stage.as_ptr() as *const u8, staged) };
        self.core.update_with_results(bytes, &mut core::iter::once(result));
    }

    /// The root's output: the core's, after the stage's bytes.
    pub(crate) fn final_output(&self) -> Output {
        if self.staged == 0 {
            return self.core.final_output();
        }
        let mut core = self.core.clone();
        core.update(self.staged_bytes());
        core.final_output()
    }

    /// Construct a new `Hasher` in a [`Mode`]: the plain hash, the keyed
    /// hash, or key derivation with a context string or a context key.
    pub fn new(mode: Mode) -> Self {
        let (key, flags) = mode.key_and_flags();
        Self::from_core(HasherCore::new_internal(&key, flags))
    }

    /// Reset the `Hasher` to its initial state.
    ///
    /// This is functionally the same as overwriting the `Hasher` with a new
    /// one, using the same key or context string if any.
    pub fn reset(&mut self) -> &mut Self {
        self.core.reset();
        self.staged = 0;
        self
    }

    /// Add the message's next piece, of any length, on the calling thread.
    ///
    /// **Speed**: pieces of any size hash at nearly the speed of whole
    /// buffers: short pieces are copied into the `Hasher` until it holds
    /// 16 KiB, which then hash at once. Pieces of 16 KiB and more, starting
    /// at a multiple of 16 KiB of the message, skip the copy (about 5% of the
    /// time on an Apple M4). For a whole message in memory, [`hash`](convenience::hash) is a
    /// little faster; for a file or reader, see
    /// [`update_reader`](#method.update_reader).
    pub fn update(&mut self, input: &[u8]) -> &mut Self {
        // A fresh hasher's first update is a one-shot call's work (a
        // Hasher per message, the digest traits): it prefetches its code
        // after a pause, as hash() does.
        #[cfg(all(blake3_neon, feature = "std"))]
        if self.count() == 0 && (CHUNK_LEN + 1..PREFETCH_BELOW).contains(&input.len()) {
            prefetch_after_pause(input.len(), self.core.chunk_state.platform, true);
        }
        self.gather(input, false)
    }

    /// [`update`](Hasher::update), using other CPU cores where that is
    /// faster: the same result, never slower. Pieces of 512 KiB and more
    /// use them, as [`hash_multithreaded`](performance::hash_multithreaded) does; shorter ones hash on the
    /// calling thread. One `Hasher` may mix the two calls. When reading
    /// the pieces keeps your thread busy, a [`stream`](performance::stream) is faster
    /// ([`Stream::submit_reader`](performance::Stream::submit_reader), or [`Stream::submit_with`](performance::Stream::submit_with)): it hashes
    /// each piece while your thread reads the next.
    ///
    /// ```
    /// # blake3_servil::initialize_multithreaded(None);
    /// let input = vec![7u8; 3 << 20];
    /// let mut hasher = blake3_servil::performance::Hasher::new(blake3_servil::Mode::Plain);
    /// for piece in input.chunks(64 * 1024) {
    ///     hasher.update_multithreaded(piece);
    /// }
    /// assert_eq!(hasher.finalize(), blake3_servil::convenience::hash(&input));
    /// ```
    #[cfg(feature = "std")]
    pub fn update_multithreaded(&mut self, input: &[u8]) -> &mut Self {
        lanes::require();
        self.gather(input, true)
    }

    /// Finalize the hash state and return the [`Hash`](struct.Hash.html) of
    /// the input.
    ///
    /// This method is idempotent. Calling it twice will give the same result.
    /// You can also add more input and finalize again.
    pub fn finalize(&self) -> Hash {
        assert_eq!(self.core.initial_chunk_counter, 0, "set_input_offset must be used with finalize_non_root",);
        self.final_output().root_hash()
    }

    /// Finalize the hash state and return an [`OutputReader`], which can
    /// supply any number of output bytes.
    ///
    /// This method is idempotent. Calling it twice will give the same result.
    /// You can also add more input and finalize again.
    ///
    /// [`OutputReader`]: struct.OutputReader.html
    pub fn finalize_xof(&self) -> OutputReader {
        assert_eq!(self.core.initial_chunk_counter, 0, "set_input_offset must be used with finalize_non_root",);
        OutputReader::new(self.final_output())
    }

    /// The number of bytes passed to [`update`](Hasher::update) so far.
    pub fn count(&self) -> u64 {
        self.core.count() + self.staged as u64
    }

    /// As [`update`](Hasher::update), but reading from a
    /// [`std::io::Read`](https://doc.rust-lang.org/std/io/trait.Read.html) implementation.
    ///
    /// [`Hasher`] implements
    /// [`std::io::Write`](https://doc.rust-lang.org/std/io/trait.Write.html), so it's possible to
    /// use [`std::io::copy`](https://doc.rust-lang.org/std/io/fn.copy.html) to update a [`Hasher`]
    /// from any reader. Unfortunately, this standard approach can limit performance, because
    /// `copy` currently uses an internal 8 KiB buffer that isn't big enough to take advantage of
    /// all SIMD instruction sets. (In particular, [AVX-512](https://en.wikipedia.org/wiki/AVX-512)
    /// needs a 16 KiB buffer.) `update_reader` avoids this performance problem and is slightly
    /// more convenient.
    ///
    /// The internal buffer size this method uses may change at any time, and it may be different
    /// for different targets. The only guarantee is that it will be large enough for all of this
    /// crate's SIMD implementations on the current platform.
    ///
    /// The most common implementer of
    /// [`std::io::Read`](https://doc.rust-lang.org/std/io/trait.Read.html) might be
    /// [`std::fs::File`](https://doc.rust-lang.org/std/fs/struct.File.html).
    ///
    /// This method requires the `std` Cargo feature, which is enabled by default.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::fs::File;
    /// # use std::io;
    /// # fn main() -> io::Result<()> {
    /// # blake3_servil::initialize();
    /// // Hash standard input.
    /// let mut hasher = blake3_servil::performance::Hasher::new(blake3_servil::Mode::Plain);
    /// hasher.update_reader(std::io::stdin().lock())?;
    /// println!("{}", hasher.finalize());
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "std")]
    pub fn update_reader(&mut self, reader: impl std::io::Read) -> std::io::Result<&mut Self> {
        io::copy_wide(reader, self)?;
        Ok(self)
    }
}

impl HasherCore {
    pub(crate) fn new_internal(key: &CVWords, flags: u8) -> Self {
        Self::new_from(key, flags, Platform::detect())
    }

    /// [`HasherCore::new_internal`] on `platform` (the self-test's way in,
    /// as [`hash_serial_from`]).
    pub(crate) fn new_from(key: &CVWords, flags: u8, platform: Platform) -> Self {
        Self { key: *key, chunk_state: ChunkState::new(key, 0, flags, platform), initial_chunk_counter: 0, cv_stack: CvStack::new() }
    }

    /// Back to the empty message, with the same key, flags, and platform.
    pub(crate) fn reset(&mut self) -> &mut Self {
        self.chunk_state = ChunkState::new(&self.key, 0, self.chunk_state.flags, self.chunk_state.platform);
        self.cv_stack.clear();
        self
    }

    // As described in push_cv() below, we do "lazy merging", delaying merges
    // until right before the next CV is about to be added (official BLAKE3's
    // reference implementation merges eagerly). Also, we aren't
    // always merging 1 chunk at a time. Instead, each CV might represent any
    // power-of-two number of chunks, as long as the smaller-above-larger stack
    // order is maintained. Instead of the "count the trailing 0-bits"
    // algorithm described in the spec (which assumes you're adding one chunk
    // at a time), we use a "count the total number of 1-bits" variant (which
    // doesn't assume that). The principle is the same: each CV that should
    // remain in the stack is represented by a 1-bit in the total number of
    // chunks (or bytes) so far.
    #[cfg(feature = "std")]
    fn merge_cv_stack(&mut self, chunk_counter: u64) {
        // Its callers (the queue's hashers, and a Hasher's held group) start
        // at offset zero; a positioned subtree's offset takes the core's path.
        debug_assert_eq!(self.initial_chunk_counter, 0, "a hasher at offset zero");
        let post_merge_stack_len = hasher::ones(chunk_counter);
        let kernels = kernels_on(&self.key, &self.chunk_state, self.chunk_state.platform, false);
        self.cv_stack.merge(&kernels, post_merge_stack_len);
    }

    /// The kernels on this Hasher's key and flags (see HasherKernels).
    #[inline(always)]
    fn kernels(&self, platform: Platform, pooled: bool) -> HasherKernels<'_> {
        kernels_on(&self.key, &self.chunk_state, platform, pooled)
    }

    // Official BLAKE3's reference implementation (reference_impl.rs) merges
    // the new CV with existing CVs from the stack before pushing it. It can,
    // because it knows more input is coming, so none of the merges are root.
    //
    // This setting is different. We want to feed as much input as possible to
    // the walk (`tree::wide`), without setting aside anything for the
    // chunk_state. If the user gives us 64 KiB, we want to parallelize over
    // all 64 KiB at once as a single subtree, if at all possible.
    //
    // This leads to two problems:
    // 1) This 64 KiB input might be the only call that ever gets made to
    //    update. In this case, the root node of the 64 KiB subtree would be
    //    the root node of the whole tree, and it would need to be ROOT
    //    finalized. We can't compress it until we know.
    // 2) This 64 KiB input might complete a larger tree, whose root node is
    //    similarly going to be the root of the whole tree. For example,
    //    maybe we have 196 KiB (that is, 128 + 64) hashed so far. We can't
    //    compress the node at the root of the 256 KiB subtree until we know
    //    how to finalize it.
    //
    // The second problem is solved with "lazy merging". That is, when we're
    // about to add a CV to the stack, we don't merge it with anything first,
    // as the reference implementation does. Instead we do merges using the *previous* CV
    // that was added, which is sitting on top of the stack, and we put the new
    // CV (unmerged) on top of the stack afterwards. This guarantees that we
    // never merge the root node until finalize().
    //
    // Solving the first problem requires an additional tool,
    // compress_subtree_to_parent_node(). That function always returns the top
    // *two* chaining values of the subtree it's compressing. We then do lazy
    // merging with each of them separately, so that the second CV will always
    // remain unmerged. (That also helps us support extendable output when
    // we're hashing an input all-at-once.)
    #[cfg(feature = "std")]
    fn push_cv(&mut self, new_cv: &CVBytes, chunk_counter: u64) {
        self.merge_cv_stack(chunk_counter);
        self.cv_stack.push(*new_cv);
    }

    /// [`update`](Hasher::update) with every whole subtree's result taken
    /// from `results`, one a whole subtree in order (in parts of
    /// [`lanes::TASK_LEN`], as [`hasher::subtree_len`] cuts them), as
    /// [`lanes::Task`] computes them.
    #[cfg(feature = "std")]
    pub(crate) fn update_with_results<'a>(&mut self, input: &[u8], results: &mut impl Iterator<Item = &'a [u8; BLOCK_LEN]>) {
        for part in input.chunks(lanes::TASK_LEN) {
            self.update_part_with_results(part, results);
        }
    }

    #[cfg(feature = "std")]
    fn update_part_with_results<'a>(&mut self, mut input: &[u8], results: &mut impl Iterator<Item = &'a [u8; BLOCK_LEN]>) {
        // Its one caller hands it a whole held group, from a group boundary,
        // where the core's chunk is empty (Hasher::take_held).
        debug_assert_eq!(self.chunk_state.count(), 0, "a held group starts where the core's chunk is empty");
        while input.len() > CHUNK_LEN {
            let len = hasher::subtree_len(self.chunk_state.chunk_counter, input.len());
            let chunks = (len / CHUNK_LEN) as u64;
            let result = results.next().expect("a result for every planned subtree");
            let counter = self.chunk_state.chunk_counter;
            if counter == 0 && len > CHUNK_LEN {
                self.push_cv(result[..OUT_LEN].try_into().unwrap(), counter);
                self.push_cv(result[OUT_LEN..].try_into().unwrap(), counter + chunks / 2);
            } else {
                self.push_cv(result[..OUT_LEN].try_into().unwrap(), counter);
            }
            self.chunk_state.chunk_counter += chunks;
            input = &input[len..];
        }
        if !input.is_empty() {
            self.chunk_state.update(input);
            self.merge_cv_stack(self.chunk_state.chunk_counter);
        }
    }

    /// `update`'s loop; with `pooled`, whole subtrees of
    /// `lanes::MIN_SPLIT_LEN` and more go to the pool.
    pub(crate) fn update_pooled(&mut self, input: &[u8], pooled: bool) -> &mut Self {
        #[cfg(not(feature = "std"))]
        let _ = pooled;
        self.assert_room(self.count(), input.len());
        // The partial chunk first (hasher::fill), then whole subtrees and the
        // last chunk (hasher::feed), as src/hasher.rs describes.
        let fill_kernels = kernels_on(&self.key, &self.chunk_state, self.chunk_state.platform, pooled);
        let rest = hasher::fill(&fill_kernels, &mut self.chunk_state, &mut self.cv_stack, self.initial_chunk_counter, input);
        if rest.is_empty() {
            return self;
        }
        // Pooled, the subtrees take the lock one at a time (children).
        let turn = platform::Sme2Turn::take(self.chunk_state.platform, !pooled && rest.len() >= SME2_SIZED_LEN);
        let kernels = kernels_on(&self.key, &self.chunk_state, turn.platform(), pooled);
        hasher::feed(&kernels, &mut self.chunk_state, &mut self.cv_stack, self.initial_chunk_counter, rest);
        self
    }

    pub(crate) fn final_output(&self) -> Output {
        hasher::final_node(&self.kernels(self.chunk_state.platform, false), &self.chunk_state, &self.cv_stack)
    }

    /// The serial update: whole subtrees now, the last chunk kept.
    pub(crate) fn update(&mut self, input: &[u8]) -> &mut Self {
        self.update_pooled(input, false)
    }

    /// Requires that `len` more bytes after `count` fit the subtree that
    /// set_input_offset started (max_subtree_len).
    pub(crate) fn assert_room(&self, count: u64, len: usize) {
        let input_offset = self.initial_chunk_counter * CHUNK_LEN as u64;
        if let Some(max) = max_subtree_len(input_offset) {
            assert!(len as u64 <= max - count, "the subtree starting at {} contains at most {} bytes (found {})", input_offset, max, count + len as u64,);
        }
    }

    /// Start the subtree at byte `offset` of the message: the tests' own
    /// (set_input_offset), since the outboard, its one caller, is gone.
    #[cfg(test)]
    pub(crate) fn set_input_offset(&mut self, offset: u64) -> &mut Self {
        assert_eq!(self.count(), 0, "hasher has already accepted input");
        assert_eq!(offset % CHUNK_LEN as u64, 0, "offset ({offset}) must be a chunk boundary (divisible by {CHUNK_LEN})",);
        let counter = offset / CHUNK_LEN as u64;
        self.chunk_state.chunk_counter = counter;
        self.initial_chunk_counter = counter;
        self
    }

    /// Bytes so far, counted from the input offset.
    pub(crate) fn count(&self) -> u64 {
        (self.chunk_state.chunk_counter - self.initial_chunk_counter) * CHUNK_LEN as u64 + self.chunk_state.count() as u64
    }
}

// Don't derive(Debug), because the state may be secret.
impl fmt::Debug for Hasher {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Hasher").field("flags", &self.core.chunk_state.flags).field("platform", &self.core.chunk_state.platform).finish()
    }
}

#[cfg(feature = "std")]
impl std::io::Write for Hasher {
    /// This is equivalent to [`update`](#method.update).
    #[inline]
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        self.update(input);
        Ok(input.len())
    }

    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for Hasher {
    fn zeroize(&mut self) {
        self.core.zeroize();
        self.stage.fill(core::mem::MaybeUninit::new(0));
        self.staged = 0;
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for HasherCore {
    fn zeroize(&mut self) {
        // Destructuring to trigger compile error as a reminder to update this impl.
        let Self { key, chunk_state, initial_chunk_counter, cv_stack } = self;

        key.zeroize();
        chunk_state.zeroize();
        initial_chunk_counter.zeroize();
        cv_stack.zeroize();
    }
}
