//! Safe compute primitives: disjoint per-thread output views ([`Out`], [`Chunks`], [`Tiles`]),
//! dispatch geometry & verification ([`Grid`], [`LaunchProof`], [`check_launch`]), and phased
//! workgroup shared memory ([`SharedMem`], [`SharedWrite`] → `.barrier()` → [`SharedRead`]) whose
//! typestate prevents read-before-barrier and write-after-barrier at compile time.

use crate::entry::{EntryDesc, MAX_THREADS, Pod, RawOut, Stage, Thread};
use crate::vec::{UVec2, UVec3, uvec2, uvec3};
use core::cell::UnsafeCell;
use core::ops::Range;

/// A 3D compute dispatch grid: `groups` workgroups, each of `threads` threads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Grid {
    pub groups: UVec3,
    pub threads: UVec3,
}

impl Grid {
    /// Creates a grid of `groups` workgroups, each of size `threads`.
    #[inline]
    pub const fn new(groups: UVec3, threads: UVec3) -> Self {
        Self { groups, threads }
    }

    /// Creates a 1D grid covering at least `n` invocations using `threads` threads per workgroup.
    #[inline]
    pub const fn cover(n: u32, threads: u32) -> Self {
        assert!(threads > 0, "Grid::cover: threads per workgroup must be > 0");
        let groups_x = if n == 0 { 0 } else { n.div_ceil(threads) };
        Self {
            groups: uvec3(groups_x, 1, 1),
            threads: uvec3(threads, 1, 1),
        }
    }

    /// Creates a 2D grid covering at least `w × h` invocations using `threads` per workgroup.
    #[inline]
    pub const fn cover_2d(w: u32, h: u32, threads: UVec2) -> Self {
        assert!(threads.x > 0 && threads.y > 0, "Grid::cover_2d: threads per workgroup must be > 0");
        let gx = if w == 0 { 0 } else { w.div_ceil(threads.x) };
        let gy = if h == 0 { 0 } else { h.div_ceil(threads.y) };
        Self {
            groups: uvec3(gx, gy, 1),
            threads: uvec3(threads.x, threads.y, 1),
        }
    }

    /// Total threads along each axis: `groups * threads`.
    #[inline]
    pub const fn total_threads(self) -> UVec3 {
        uvec3(
            self.groups.x * self.threads.x,
            self.groups.y * self.threads.y,
            self.groups.z * self.threads.z,
        )
    }

    /// Total threads per workgroup (`threads.x * threads.y * threads.z`).
    #[inline]
    pub const fn workgroup_size(self) -> u64 {
        (self.threads.x as u64) * (self.threads.y as u64) * (self.threads.z as u64)
    }

    /// Total number of invocations across the entire grid.
    #[inline]
    pub const fn thread_count(self) -> u64 {
        let tot = self.total_threads();
        (tot.x as u64) * (tot.y as u64) * (tot.z as u64)
    }
}

/// Disjoint 1D chunk partition: thread `i = t.global().x` exclusively owns `[i * N .. (i + 1) * N]`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Chunks<const N: usize>;

impl<const N: usize> Chunks<N> {
    #[inline]
    pub const fn new() -> Self {
        assert!(N > 0, "Chunks<N>: N must be > 0");
        Self
    }

    /// The disjoint element index range owned by `t`.
    #[inline]
    pub const fn range_for(&self, t: &Thread) -> Range<usize> {
        let start = (t.global().x as usize) * N;
        start..start + N
    }

    /// Minimum output buffer length required so every thread in `grid` stays in bounds.
    #[inline]
    pub const fn required_len(&self, grid: Grid) -> usize {
        (grid.total_threads().x as usize) * N
    }
}

/// Disjoint 2D tile partition: thread `(gx, gy)` exclusively owns the `W × H` tile starting at
/// `(gx * W, gy * H)`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Tiles<const W: u32, const H: u32>;

impl<const W: u32, const H: u32> Tiles<W, H> {
    #[inline]
    pub const fn new() -> Self {
        assert!(W > 0 && H > 0, "Tiles<W, H>: tile dimensions must be > 0");
        Self
    }

    /// Top-left pixel origin of the tile owned by `t`.
    #[inline]
    pub const fn origin_for(&self, t: &Thread) -> UVec2 {
        let g = t.global();
        uvec2(g.x * W, g.y * H)
    }

    /// Row-major buffer index for local tile offset `(lx, ly)` in an image of `width × height`.
    /// Returns `None` if `(lx, ly)` is outside `[0..W) × [0..H)` or the pixel lies outside the image.
    #[inline]
    pub const fn index_for(&self, t: &Thread, lx: u32, ly: u32, width: u32, height: u32) -> Option<usize> {
        if lx >= W || ly >= H {
            return None;
        }
        let o = self.origin_for(t);
        let px = o.x + lx;
        let py = o.y + ly;
        if px >= width || py >= height {
            return None;
        }
        Some((py as usize) * (width as usize) + (px as usize))
    }

    /// Minimum output buffer length required for a `width × height` image.
    #[inline]
    pub const fn required_len(&self, width: u32, height: u32) -> usize {
        (width as usize) * (height as usize)
    }
}

/// Reasons a compute launch failed pre-dispatch verification.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LaunchError {
    NotComputeStage,
    ZeroWorkgroupAxis,
    TooManyThreadsPerGroup { got: u64, max: u32 },
    WorkgroupMismatch { expected: [u32; 3], got: [u32; 3] },
    OutputTooSmall { required: usize, actual: usize },
}

/// Unforgeable proof token produced by [`check_launch`] or [`check_launch_grid`] certifying that
/// the dispatch grid and output buffer satisfy all stage and bounds invariants.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LaunchProof {
    grid: Grid,
    output_len: usize,
}

impl LaunchProof {
    #[inline]
    pub const fn grid(&self) -> Grid {
        self.grid
    }

    #[inline]
    pub const fn output_len(&self) -> usize {
        self.output_len
    }
}

/// Verifies that `grid` matches `entry`'s compute workgroup size, respects [`MAX_THREADS`], and
/// that an output buffer of `output_len` elements covers every invocation in `grid`.
pub const fn check_launch(entry: &EntryDesc, grid: Grid, output_len: usize) -> Result<LaunchProof, LaunchError> {
    let Stage::Compute { x, y, z } = entry.stage else {
        return Err(LaunchError::NotComputeStage);
    };
    let got = [grid.threads.x, grid.threads.y, grid.threads.z];
    let expected = [x, y, z];
    if got[0] != expected[0] || got[1] != expected[1] || got[2] != expected[2] {
        return Err(LaunchError::WorkgroupMismatch { expected, got });
    }
    check_launch_grid(grid, output_len)
}

/// Verifies grid validity (`threads` non-zero, `<= MAX_THREADS`) and that `output_len` is at
/// least `grid.thread_count()`.
pub const fn check_launch_grid(grid: Grid, output_len: usize) -> Result<LaunchProof, LaunchError> {
    if grid.threads.x == 0 || grid.threads.y == 0 || grid.threads.z == 0 {
        return Err(LaunchError::ZeroWorkgroupAxis);
    }
    let wg = grid.workgroup_size();
    if wg > MAX_THREADS as u64 {
        return Err(LaunchError::TooManyThreadsPerGroup { got: wg, max: MAX_THREADS });
    }
    let required = grid.thread_count();
    if (output_len as u64) < required {
        return Err(LaunchError::OutputTooSmall { required: required as usize, actual: output_len });
    }
    Ok(LaunchProof { grid, output_len })
}

/// Safe disjoint per-thread output view backed by a [`RawOut<T>`].
///
/// Every write is keyed on an unforgeable [`&Thread`], guaranteeing disjoint element ownership
/// across invocations without requiring `unsafe` at call sites.
pub struct Out<'a, T: Pod> {
    raw: &'a RawOut<T>,
}

impl<'a, T: Pod> Out<'a, T> {
    /// Wraps a [`RawOut<T>`] as a safe disjoint per-thread output view.
    #[inline]
    pub const fn new(raw: &'a RawOut<T>) -> Self {
        Self { raw }
    }

    /// Creates a safe disjoint output view directly from a host mutable slice.
    #[inline]
    pub fn from_slice(slice: &'a mut [T]) -> Self {
        Self { raw: RawOut::from_slice(slice) }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Writes `value` at this thread's 1D index `t.global().x`. Panics if out of bounds.
    #[inline]
    pub fn write(&self, t: &Thread, value: T) {
        let i = t.global().x as usize;
        // SAFETY: `Thread` is unforgeable and unique per invocation, so index `t.global().x` is
        // disjoint across all threads in a 1D dispatch; `RawOut::write` enforces bounds.
        unsafe { self.raw.write(i, value) };
    }

    /// Writes `value` at this thread's 1D index `t.global().x` if in bounds, returning whether the
    /// write occurred (convenient for tail-padded grids from [`Grid::cover`]).
    #[inline]
    pub fn try_write(&self, t: &Thread, value: T) -> bool {
        let i = t.global().x as usize;
        if i >= self.raw.len() {
            return false;
        }
        // SAFETY: `Thread` is unique per invocation and `i < self.raw.len()` was just checked.
        unsafe { self.raw.write(i, value) };
        true
    }

    /// Reads this thread's own 1D element `t.global().x`.
    #[inline]
    pub fn read(&self, t: &Thread) -> T {
        let i = t.global().x as usize;
        // SAFETY: Index `t.global().x` is owned exclusively by this thread; bounds checked inside.
        unsafe { self.raw.read(i) }
    }

    /// Writes `value` at this thread's 2D index `(t.global().x, t.global().y)` in an image of
    /// `width × height`, returning `false` if outside bounds.
    #[inline]
    pub fn write_2d(&self, t: &Thread, width: u32, height: u32, value: T) -> bool {
        let g = t.global();
        if g.x >= width || g.y >= height {
            return false;
        }
        let idx = (g.y as usize) * (width as usize) + (g.x as usize);
        if idx >= self.raw.len() {
            return false;
        }
        // SAFETY: Each 2D thread `(g.x, g.y)` maps injectively to `g.y * width + g.x`.
        unsafe { self.raw.write(idx, value) };
        true
    }

    /// Writes `value` at `offset` within this thread's exclusive [`Chunks<N>`] slice.
    #[inline]
    pub fn write_chunk<const N: usize>(&self, t: &Thread, chunks: Chunks<N>, offset: usize, value: T) {
        assert!(offset < N, "Out::write_chunk offset {offset} out of chunk size {N}");
        let idx = chunks.range_for(t).start + offset;
        // SAFETY: Distinct threads own disjoint ranges `[id * N .. (id + 1) * N)`.
        unsafe { self.raw.write(idx, value) };
    }

    /// Writes `value` at local offset `(lx, ly)` within this thread's exclusive [`Tiles<W, H>`]
    /// tile in an image of `width × height`.
    #[inline]
    pub fn write_tile<const W: u32, const H: u32>(
        &self,
        t: &Thread,
        tiles: Tiles<W, H>,
        lx: u32,
        ly: u32,
        width: u32,
        height: u32,
        value: T,
    ) -> bool {
        let Some(idx) = tiles.index_for(t, lx, ly, width, height) else {
            return false;
        };
        if idx >= self.raw.len() {
            return false;
        }
        // SAFETY: Distinct 2D threads own non-overlapping `W × H` tiles.
        unsafe { self.raw.write(idx, value) };
        true
    }

    /// Writes `value` at the linear index guaranteed by a [`LaunchProof`].
    #[inline]
    pub fn write_proven(&self, t: &Thread, proof: &LaunchProof, value: T) {
        let tot = proof.grid.total_threads();
        let g = t.global();
        let idx = ((g.z * tot.y + g.y) * tot.x + g.x) as usize;
        assert!(idx < proof.output_len && idx < self.raw.len(), "LaunchProof output mismatch");
        // SAFETY: Every thread's 3D global coordinate maps injectively to `idx`.
        unsafe { self.raw.write(idx, value) };
    }
}

/// Workgroup-shared memory array of `N` elements of `T`, accessed through phased typestates
/// ([`SharedWrite`] → `.barrier()` → [`SharedRead`]) so read-before-barrier and write-after-barrier
/// are impossible at compile time.
pub struct SharedMem<T, const N: usize> {
    slots: [UnsafeCell<T>; N],
}

impl<T: Copy, const N: usize> SharedMem<T, N> {
    /// Allocates stack/workgroup shared memory initialized to `init` (zero heap allocations).
    #[inline]
    pub fn new(init: T) -> Self {
        Self { slots: core::array::from_fn(|_| UnsafeCell::new(init)) }
    }

    /// Enters the write phase for this workgroup.
    #[inline]
    pub fn begin_write(&self) -> SharedWrite<'_, T, N> {
        SharedWrite { mem: self }
    }

    /// Number of elements in shared memory.
    #[inline]
    pub const fn len(&self) -> usize {
        N
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        N == 0
    }
}

/// Write-only phase of workgroup shared memory: each thread may only write its own slot via
/// [`Self::write_own`], and cannot read any slot until [`Self::barrier`] transitions to
/// [`SharedRead`].
pub struct SharedWrite<'a, T, const N: usize> {
    mem: &'a SharedMem<T, N>,
}

impl<'a, T: Copy, const N: usize> SharedWrite<'a, T, N> {
    /// Writes `val` to this thread's own 1D local slot `t.local().x`.
    #[inline]
    pub fn write_own(&self, t: &Thread, val: T) {
        let i = t.local().x as usize;
        assert!(i < N, "SharedWrite::write_own index {i} out of bounds for N={N}");
        let ptr = self.mem.slots[i].get();
        // SAFETY: Each thread in the workgroup has a unique `t.local().x` and no reads are
        // permitted while in the `SharedWrite` typestate.
        unsafe { *ptr = val; }
    }

    /// Writes `val` to this thread's own multi-dimensional local slot within `threads`.
    #[inline]
    pub fn write_own_nd(&self, t: &Thread, threads: UVec3, val: T) {
        let i = t.local_index(threads);
        assert!(i < N, "SharedWrite::write_own_nd index {i} out of bounds for N={N}");
        let ptr = self.mem.slots[i].get();
        // SAFETY: Each thread in the workgroup has a unique linear local index and no reads are
        // permitted while in the `SharedWrite` typestate.
        unsafe { *ptr = val; }
    }

    /// Workgroup barrier: consumes write access and transitions to [`SharedRead`].
    #[inline]
    pub fn barrier(self) -> SharedRead<'a, T, N> {
        workgroup_barrier();
        SharedRead { mem: self.mem }
    }
}

/// Read-only phase of workgroup shared memory: every thread may safely read any slot `0..N`, and
/// cannot write until [`Self::barrier`] transitions back to [`SharedWrite`].
pub struct SharedRead<'a, T, const N: usize> {
    mem: &'a SharedMem<T, N>,
}

impl<'a, T: Copy, const N: usize> SharedRead<'a, T, N> {
    /// Reads slot `idx` written before the preceding barrier.
    #[inline]
    pub fn read(&self, idx: usize) -> T {
        assert!(idx < N, "SharedRead::read index {idx} out of bounds for N={N}");
        let ptr = self.mem.slots[idx].get();
        // SAFETY: All writes completed before the barrier and no writes are permitted while in
        // `SharedRead`.
        unsafe { *ptr }
    }

    /// Bounds-checked read returning `None` if `idx >= N`.
    #[inline]
    pub fn try_read(&self, idx: usize) -> Option<T> {
        if idx >= N {
            return None;
        }
        Some(self.read(idx))
    }

    #[inline]
    pub const fn len(&self) -> usize {
        N
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        N == 0
    }

    /// Workgroup barrier: consumes read access and transitions back to [`SharedWrite`] for
    /// multi-pass algorithms (e.g. parallel reductions / scans).
    #[inline]
    pub fn barrier(self) -> SharedWrite<'a, T, N> {
        workgroup_barrier();
        SharedWrite { mem: self.mem }
    }
}

/// Device workgroup memory/execution barrier marker (no-op on single-threaded deterministic CPU
/// phase runner; lowered to `threadgroup_barrier` / `workgroupBarrier` on device).
#[inline(never)]
pub fn workgroup_barrier() {
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
