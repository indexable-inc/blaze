//! `Gpu`, the capability to use a graphics device (docs/v2/plan/design/std.md, "Gpu, video and
//! images"):
//!
//! - **Capability authority**: a hardware device comes only from [`Io::gpu`](crate::io::Io::gpu)
//!   (or [`Io::need_gpu`](crate::io::Io::need_gpu)); [`Gpu::cpu`] is always available as the
//!   deterministic CPU provider used by tests, simulations, and CPU fallback paths (plus
//!   [`Gpu::cpu_discrete`] to model discrete host/device dirty-range coherence on the CPU).
//!   Hardware video encoding ([`crate::video::Encoder::hardware`]) takes `&Gpu` to prove the
//!   program was handed a device.
//! - **Narrowing**: [`Gpu::with_budget`] tightens the device's memory budget and never widens it;
//!   narrowed handles account against both their own limit and every enclosing limit.
//! - **Explicit allocators**: [`Buffer<T, A>`], [`DeviceBuffer<T, A>`], and [`RenderTarget<A>`]
//!   are backed by a caller-supplied [`core::alloc::Allocator`] (such as `cx.frame()`, an
//!   [`crate::mem::Arena`], or [`crate::mem::Heap`]) and counted against the `Gpu` budget, with
//!   automatic byte release on drop.
//! - **Unified vs. discrete memory**: [`MemoryKind::Unified`] buffers require no host/device
//!   copies (`dirty_range` stays `None`); [`MemoryKind::Discrete`] buffers record dirty element
//!   ranges on mutable host access until synced via [`Buffer::mark_synced`]. [`DeviceBuffer`]
//!   exposes no host slice and transfers explicitly via [`DeviceBuffer::upload_from`] and
//!   [`DeviceBuffer::download_into`].
//! - **Scoped compute & offscreen rendering**: [`Gpu::scope`] and [`RenderTarget::fill_shader`]
//!   execute deterministically and check [`Cx`] cancellation and deadlines between workgroups
//!   and scanlines.

use alloc::sync::Arc;
use core::{
    alloc::Allocator,
    fmt,
    mem::size_of,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_own::Vec as OwnVec;

use crate::{Stopped, cx::Cx};

/// Which provider backs a [`Gpu`] capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// Deterministic CPU compute/render provider (always available).
    Cpu,
    /// Native macOS Metal device (and its media engine for hardware video encoding).
    Metal,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Cpu => f.write_str("cpu"),
            Backend::Metal => f.write_str("metal"),
        }
    }
}

/// Host/device memory coherence model of a [`Gpu`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoryKind {
    /// Host and device address the same memory (e.g., Apple silicon, CPU provider); host writes
    /// need no upload copy.
    Unified,
    /// Separate host and device memory (a discrete card); host writes mark dirty ranges that
    /// must be synced to the device before use.
    Discrete,
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryKind::Unified => f.write_str("unified"),
            MemoryKind::Discrete => f.write_str("discrete"),
        }
    }
}

/// Typed failures from [`Gpu`] budgeting, allocation, dispatch, and offscreen rendering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GpuError {
    /// An allocation would exceed the capability's (or an enclosing capability's) byte budget.
    BudgetExceeded { limit: u64, used: u64, asked: u64 },
    /// [`Gpu::with_budget`] was asked for a limit higher than the capability's current budget.
    BudgetWidened { current: u64, requested: u64 },
    /// The caller's allocator could not satisfy the allocation (or its byte size overflowed).
    OutOfMemory { asked: u64 },
    /// A compute [`Grid`] had a zero dimension or overflowed thread coordinates / total count.
    InvalidGrid { groups: [u32; 3], threads: [u32; 3] },
    /// A [`RenderTarget`] had a zero width or height, or its byte size overflowed.
    InvalidDimensions { width: u32, height: u32 },
    /// A host/device transfer slice length did not match the target buffer or range.
    LengthMismatch { expected: usize, actual: usize },
    /// The task's [`Cx`] was cancelled before or during execution.
    Cancelled,
    /// The task's [`Cx`] deadline passed before or during execution.
    Deadline,
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::BudgetExceeded { limit, used, asked } => {
                write!(f, "gpu budget exceeded: asked {asked} bytes with {used} of {limit} bytes used")
            }
            GpuError::BudgetWidened { current, requested } => {
                write!(f, "gpu budget cannot be widened from {current} bytes to {requested} bytes")
            }
            GpuError::OutOfMemory { asked } => {
                write!(f, "gpu allocator out of memory for {asked} bytes")
            }
            GpuError::InvalidGrid { groups, threads } => {
                write!(f, "invalid gpu dispatch grid: groups={groups:?}, threads={threads:?}")
            }
            GpuError::InvalidDimensions { width, height } => {
                write!(f, "invalid gpu render target dimensions: {width}x{height}")
            }
            GpuError::LengthMismatch { expected, actual } => {
                write!(f, "gpu buffer transfer length mismatch: expected {expected} elements, got {actual}")
            }
            GpuError::Cancelled => f.write_str("gpu operation cancelled"),
            GpuError::Deadline => f.write_str("gpu operation deadline exceeded"),
        }
    }
}

impl core::error::Error for GpuError {}

/// Lock-free hierarchical byte accounting for [`Gpu`] capabilities and their narrowed children.
struct BudgetNode {
    /// Maximum live bytes allowed at this node (`None` = unlimited).
    limit: Option<u64>,
    /// Currently live bytes allocated through this node or any narrowed child.
    used: AtomicU64,
    /// Enclosing budget node this capability was narrowed from, if any.
    outer: Option<Arc<BudgetNode>>,
}

impl BudgetNode {
    #[blaze::nonblocking]
    fn new(limit: Option<u64>, outer: Option<Arc<BudgetNode>>) -> Arc<BudgetNode> {
        // alloc: once per `Gpu` root or `Gpu::with_budget` narrowing.
        Arc::new(BudgetNode {
            limit,
            used: AtomicU64::new(0),
            outer,
        })
    }

    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn try_reserve_here(&self, bytes: u64) -> Result<(), GpuError> {
        if bytes == 0 {
            return Ok(());
        }
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return Err(GpuError::BudgetExceeded {
                    limit: self.limit.unwrap_or(u64::MAX),
                    used: current,
                    asked: bytes,
                });
            };
            if let Some(limit) = self.limit
                && next > limit
            {
                return Err(GpuError::BudgetExceeded {
                    limit,
                    used: current,
                    asked: bytes,
                });
            }
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn try_reserve(&self, bytes: u64) -> Result<(), GpuError> {
        if bytes == 0 {
            return Ok(());
        }
        self.try_reserve_here(bytes)?;
        if let Some(outer) = &self.outer
            && let Err(err) = outer.try_reserve(bytes)
        {
            self.used.fetch_sub(bytes, Ordering::AcqRel);
            return Err(err);
        }
        Ok(())
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn release(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut at = Some(self);
        while let Some(node) = at {
            node.used.fetch_sub(bytes, Ordering::AcqRel);
            at = node.outer.as_deref();
        }
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn remaining(&self) -> Option<u64> {
        let mut min_rem: Option<u64> = None;
        let mut at = Some(self);
        while let Some(node) = at {
            if let Some(limit) = node.limit {
                let rem = limit.saturating_sub(node.used.load(Ordering::Acquire));
                min_rem = Some(match min_rem {
                    Some(prev) => prev.min(rem),
                    None => rem,
                });
            }
            at = node.outer.as_deref();
        }
        min_rem
    }
}

/// RAII byte reservation against a [`BudgetNode`] chain; releases automatically on drop.
struct BudgetLease {
    node: Arc<BudgetNode>,
    bytes: u64,
}

impl BudgetLease {
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn acquire(node: &Arc<BudgetNode>, bytes: u64) -> Result<BudgetLease, GpuError> {
        node.try_reserve(bytes)?;
        Ok(BudgetLease {
            node: Arc::clone(node),
            bytes,
        })
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        self.node.release(self.bytes);
    }
}

/// Checks `cx` for cancellation or an expired deadline.
#[blaze::noalloc]
#[blaze::nonblocking]
fn check_cx(cx: &Cx) -> Result<(), GpuError> {
    match cx.stopped(cx.io()) {
        Ok(()) => Ok(()),
        Err(Stopped::Cancelled { .. }) => Err(GpuError::Cancelled),
        Err(Stopped::Deadline { .. }) => Err(GpuError::Deadline),
        Err(Stopped::OutOfMemory { asked, .. }) => Err(GpuError::OutOfMemory { asked: asked as u64 }),
        Err(Stopped::Stalled { .. }) => Err(GpuError::Cancelled),
    }
}

/// Computes `len * size_of::<T>()` as `u64`, checking for overflow.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
fn checked_byte_len<T>(len: usize) -> Result<u64, GpuError> {
    let elem = size_of::<T>();
    let total = len
        .checked_mul(elem)
        .ok_or(GpuError::OutOfMemory { asked: u64::MAX })?;
    u64::try_from(total).map_err(|_| GpuError::OutOfMemory { asked: u64::MAX })
}

/// A graphics/compute device capability.
///
/// Hardware devices come only from [`Io::gpu`](crate::io::Io::gpu) or
/// [`Io::need_gpu`](crate::io::Io::need_gpu). [`Gpu::cpu`] constructs the deterministic CPU
/// provider available everywhere (including tests and the simulation). Cloning a `Gpu` copies
/// the authority it holds and shares its budget accounting; it never widens authority.
pub struct Gpu {
    backend: Backend,
    memory_kind: MemoryKind,
    budget: Arc<BudgetNode>,
}

// `Gpu` is safe to share across tasks and threads (atomic budget accounting only).
const _: () = {
    const fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<Gpu>();
};

impl Clone for Gpu {
    fn clone(&self) -> Gpu {
        Gpu {
            backend: self.backend,
            memory_kind: self.memory_kind,
            budget: Arc::clone(&self.budget),
        }
    }
}

impl fmt::Debug for Gpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gpu")
            .field("backend", &self.backend)
            .field("memory_kind", &self.memory_kind)
            .field("budget_bytes", &self.budget.limit)
            .field("used_bytes", &self.budget.used())
            .finish()
    }
}

impl Gpu {
    /// What `Io::gpu` gives: a Metal device on a native macOS world (whose media engine the
    /// hardware video encoder runs on); none in the simulation, which has no hardware device,
    /// nor on other targets until their providers exist.
    #[blaze::nonblocking]
    pub(crate) fn offered(native: bool) -> Option<Gpu> {
        if native && cfg!(target_os = "macos") {
            let memory_kind = if cfg!(target_arch = "aarch64") {
                MemoryKind::Unified
            } else {
                MemoryKind::Discrete
            };
            Some(Gpu {
                backend: Backend::Metal,
                memory_kind,
                budget: BudgetNode::new(None, None),
            })
        } else {
            None
        }
    }

    /// Always-available deterministic CPU compute/render provider with [`MemoryKind::Unified`]
    /// and no default memory cap.
    #[blaze::nonblocking]
    pub fn cpu() -> Gpu {
        Gpu {
            backend: Backend::Cpu,
            memory_kind: MemoryKind::Unified,
            budget: BudgetNode::new(None, None),
        }
    }

    /// Deterministic CPU provider modeling [`MemoryKind::Discrete`] host/device dirty-range
    /// tracking, with no default memory cap.
    #[blaze::nonblocking]
    pub fn cpu_discrete() -> Gpu {
        Gpu {
            backend: Backend::Cpu,
            memory_kind: MemoryKind::Discrete,
            budget: BudgetNode::new(None, None),
        }
    }

    /// The provider backing this device.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Whether this device uses unified or discrete host/device memory.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn memory_kind(&self) -> MemoryKind {
        self.memory_kind
    }

    /// This capability's own byte limit (`None` when unbounded).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn budget_bytes(&self) -> Option<u64> {
        self.budget.limit
    }

    /// Live bytes currently allocated through this capability (and any children narrowed from
    /// it).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn used_bytes(&self) -> u64 {
        self.budget.used()
    }

    /// Effective remaining bytes available under this capability and all enclosing budgets
    /// (`None` if neither this capability nor any enclosing one has a limit).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn remaining_bytes(&self) -> Option<u64> {
        self.budget.remaining()
    }

    /// Narrows this capability to a byte budget of at most `max_bytes`.
    ///
    /// Returns [`GpuError::BudgetWidened`] if `self` already has a budget strictly smaller than
    /// `max_bytes`. Allocations made through the returned handle count against both the new
    /// limit and `self`'s budget chain.
    #[blaze::nonblocking]
    pub fn with_budget(&self, max_bytes: u64) -> Result<Gpu, GpuError> {
        if let Some(current) = self.budget.limit
            && max_bytes > current
        {
            return Err(GpuError::BudgetWidened {
                current,
                requested: max_bytes,
            });
        }
        Ok(Gpu {
            backend: self.backend,
            memory_kind: self.memory_kind,
            budget: BudgetNode::new(Some(max_bytes), Some(Arc::clone(&self.budget))),
        })
    }

    fn alloc_buffer_in<T, A: Allocator>(
        &self,
        len: usize,
        alloc: A,
        init: impl FnOnce(&mut OwnVec<T, A>, u64) -> Result<(), GpuError>,
    ) -> Result<Buffer<T, A>, GpuError> {
        let byte_len = checked_byte_len::<T>(len)?;
        let lease = BudgetLease::acquire(&self.budget, byte_len)?;
        let mut data = OwnVec::try_with_capacity_in(len, alloc)
            .map_err(|_| GpuError::OutOfMemory { asked: byte_len })?;
        init(&mut data, byte_len)?;
        Ok(Buffer {
            data,
            memory_kind: self.memory_kind,
            dirty: None,
            _lease: lease,
        })
    }

    /// Allocates a host-accessible [`Buffer`] of `len` elements initialized to clones of `fill`
    /// inside `alloc`, counted against this device's budget.
    pub fn buffer_in<T: Clone, A: Allocator>(
        &self,
        len: usize,
        fill: T,
        alloc: A,
    ) -> Result<Buffer<T, A>, GpuError> {
        self.alloc_buffer_in(len, alloc, |data, _| {
            data.resize(len, fill);
            Ok(())
        })
    }

    /// Allocates a host-accessible [`Buffer`] of `len` default/zeroed elements inside `alloc`,
    /// counted against this device's budget.
    pub fn buffer_zeroed_in<T: Default, A: Allocator>(
        &self,
        len: usize,
        alloc: A,
    ) -> Result<Buffer<T, A>, GpuError> {
        self.alloc_buffer_in(len, alloc, |data, _| {
            data.resize_with(len, T::default);
            Ok(())
        })
    }

    /// Allocates a host-accessible [`Buffer`] initialized from `src` inside `alloc`, counted
    /// against this device's budget.
    pub fn buffer_from_slice_in<T: Clone, A: Allocator>(
        &self,
        src: &[T],
        alloc: A,
    ) -> Result<Buffer<T, A>, GpuError> {
        self.alloc_buffer_in(src.len(), alloc, |data, byte_len| {
            data.try_extend_from_slice(src)
                .map_err(|_| GpuError::OutOfMemory { asked: byte_len })
        })
    }

    /// Allocates a device-only [`DeviceBuffer`] of `len` elements initialized to clones of
    /// `fill` inside `alloc`, counted against this device's budget.
    pub fn device_buffer_in<T: Clone, A: Allocator>(
        &self,
        len: usize,
        fill: T,
        alloc: A,
    ) -> Result<DeviceBuffer<T, A>, GpuError> {
        self.buffer_in(len, fill, alloc).map(Buffer::into_device)
    }

    /// Allocates a device-only [`DeviceBuffer`] of `len` default/zeroed elements inside `alloc`,
    /// counted against this device's budget.
    pub fn device_buffer_zeroed_in<T: Default, A: Allocator>(
        &self,
        len: usize,
        alloc: A,
    ) -> Result<DeviceBuffer<T, A>, GpuError> {
        self.buffer_zeroed_in(len, alloc).map(Buffer::into_device)
    }

    /// Allocates a device-only [`DeviceBuffer`] initialized from `src` inside `alloc`, counted
    /// against this device's budget.
    pub fn device_buffer_from_slice_in<T: Clone, A: Allocator>(
        &self,
        src: &[T],
        alloc: A,
    ) -> Result<DeviceBuffer<T, A>, GpuError> {
        self.buffer_from_slice_in(src, alloc).map(Buffer::into_device)
    }

    /// Allocates an offscreen RGBA8 [`RenderTarget`] of `width` × `height` pixels in `alloc`,
    /// zero-initialized and counted against this device's budget.
    pub fn render_target_in<A: Allocator>(
        &self,
        width: u32,
        height: u32,
        alloc: A,
    ) -> Result<RenderTarget<A>, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::InvalidDimensions { width, height });
        }
        let pixel_count = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
            .ok_or(GpuError::InvalidDimensions { width, height })?;
        let byte_len_usize = pixel_count
            .checked_mul(4)
            .ok_or(GpuError::InvalidDimensions { width, height })?;
        let byte_len = u64::try_from(byte_len_usize)
            .map_err(|_| GpuError::InvalidDimensions { width, height })?;
        let lease = BudgetLease::acquire(&self.budget, byte_len)?;
        let mut pixels = OwnVec::try_with_capacity_in(byte_len_usize, alloc)
            .map_err(|_| GpuError::OutOfMemory { asked: byte_len })?;
        pixels.resize(byte_len_usize, 0);
        Ok(RenderTarget {
            width,
            height,
            pixels,
            memory_kind: self.memory_kind,
            _lease: lease,
        })
    }

    /// Runs a scoped GPU command pass under `cx`.
    ///
    /// Checks `cx` for cancellation and deadline expiration before entering `f`, and again
    /// before and between workgroups of every [`GpuScope::dispatch`].
    pub fn scope<'g, 'cx, R>(
        &'g self,
        cx: &'cx Cx,
        f: impl FnOnce(&mut GpuScope<'g, 'cx>) -> Result<R, GpuError>,
    ) -> Result<R, GpuError> {
        check_cx(cx)?;
        let mut scope = GpuScope {
            gpu: self,
            cx,
            dispatches: 0,
            threads_executed: 0,
        };
        f(&mut scope)
    }
}

/// A host-accessible GPU buffer allocated in a caller-provided allocator `A` and counted against
/// the [`Gpu`] capability's memory budget.
pub struct Buffer<T, A: Allocator> {
    data: OwnVec<T, A>,
    memory_kind: MemoryKind,
    /// Dirty element range `[lo, hi)` pending upload on [`MemoryKind::Discrete`] devices.
    dirty: Option<(usize, usize)>,
    _lease: BudgetLease,
}

impl<T, A: Allocator> Buffer<T, A> {
    /// Number of elements in the buffer.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the buffer holds zero elements.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Byte size charged against the [`Gpu`] budget.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn byte_len(&self) -> u64 {
        self._lease.bytes()
    }

    /// Memory coherence kind of the device that owns this buffer.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn memory_kind(&self) -> MemoryKind {
        self.memory_kind
    }

    /// Backing allocator handed in at construction.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn allocator(&self) -> &A {
        self.data.allocator()
    }

    /// Shared host view of the buffer's elements.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn as_slice(&self) -> &[T] {
        self.data.as_slice()
    }

    /// Mutable host view of the buffer's elements.
    ///
    /// On [`MemoryKind::Discrete`] devices, marks `(0, self.len())` dirty (when non-empty).
    /// On [`MemoryKind::Unified`] devices, leaves [`Buffer::dirty_range`] as `None`.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        let len = self.data.len();
        if self.memory_kind == MemoryKind::Discrete && len > 0 {
            self.mark_dirty(0, len);
        }
        self.data.as_mut_slice()
    }

    /// Mutable host view of a sub-range of the buffer's elements.
    ///
    /// Returns `None` if `range` is out of bounds or inverted. On [`MemoryKind::Discrete`]
    /// devices, merges `(range.start, range.end)` into [`Buffer::dirty_range`] when non-empty.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn as_mut_range(&mut self, range: Range<usize>) -> Option<&mut [T]> {
        let len = self.data.len();
        if range.start > range.end || range.end > len {
            return None;
        }
        if self.memory_kind == MemoryKind::Discrete && range.start < range.end {
            self.mark_dirty(range.start, range.end);
        }
        self.data.as_mut_slice().get_mut(range)
    }

    /// Half-open element range `(lo, hi)` dirtied by host writes on a discrete device, or `None`
    /// if clean (always `None` on [`MemoryKind::Unified`]).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn dirty_range(&self) -> Option<(usize, usize)> {
        self.dirty
    }

    /// Clears the pending dirty range once host writes have been uploaded to the device.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn mark_synced(&mut self) {
        self.dirty = None;
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn mark_dirty(&mut self, lo: usize, hi: usize) {
        self.dirty = Some(match self.dirty {
            Some((prev_lo, prev_hi)) => (prev_lo.min(lo), prev_hi.max(hi)),
            None => (lo, hi),
        });
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn into_device(self) -> DeviceBuffer<T, A> {
        DeviceBuffer {
            data: self.data,
            memory_kind: self.memory_kind,
            _lease: self._lease,
        }
    }
}

impl<T, A: Allocator> fmt::Debug for Buffer<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("len", &self.data.len())
            .field("byte_len", &self._lease.bytes())
            .field("memory_kind", &self.memory_kind)
            .field("dirty", &self.dirty)
            .finish()
    }
}

/// A device-only GPU buffer with no direct host slice access (for kernel intermediates or
/// discrete VRAM allocations). Data moves in and out only through explicit
/// [`DeviceBuffer::upload_from`] and [`DeviceBuffer::download_into`] transfers.
pub struct DeviceBuffer<T, A: Allocator> {
    data: OwnVec<T, A>,
    memory_kind: MemoryKind,
    _lease: BudgetLease,
}

impl<T, A: Allocator> DeviceBuffer<T, A> {
    /// Number of elements in the buffer.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the buffer holds zero elements.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Byte size charged against the [`Gpu`] budget.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn byte_len(&self) -> u64 {
        self._lease.bytes()
    }

    /// Memory coherence kind of the device that owns this buffer.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn memory_kind(&self) -> MemoryKind {
        self.memory_kind
    }

    /// Backing allocator handed in at construction.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn allocator(&self) -> &A {
        self.data.allocator()
    }

    /// Explicitly uploads `src` into this device buffer.
    ///
    /// Returns [`GpuError::LengthMismatch`] if `src.len() != self.len()`.
    pub fn upload_from(&mut self, src: &[T]) -> Result<(), GpuError>
    where
        T: Clone,
    {
        if src.len() != self.data.len() {
            return Err(GpuError::LengthMismatch {
                expected: self.data.len(),
                actual: src.len(),
            });
        }
        self.data.as_mut_slice().clone_from_slice(src);
        Ok(())
    }

    /// Explicitly uploads `src` starting at element `offset`.
    ///
    /// Returns [`GpuError::LengthMismatch`] if `offset + src.len() > self.len()`.
    pub fn upload_range(&mut self, offset: usize, src: &[T]) -> Result<(), GpuError>
    where
        T: Clone,
    {
        let Some(end) = offset.checked_add(src.len()) else {
            return Err(GpuError::LengthMismatch {
                expected: self.data.len().saturating_sub(offset),
                actual: src.len(),
            });
        };
        if end > self.data.len() {
            return Err(GpuError::LengthMismatch {
                expected: self.data.len().saturating_sub(offset),
                actual: src.len(),
            });
        }
        self.data.as_mut_slice()[offset..end].clone_from_slice(src);
        Ok(())
    }

    /// Explicitly downloads the entire device buffer into `dst`.
    ///
    /// Returns [`GpuError::LengthMismatch`] if `dst.len() != self.len()`.
    pub fn download_into(&self, dst: &mut [T]) -> Result<(), GpuError>
    where
        T: Clone,
    {
        if dst.len() != self.data.len() {
            return Err(GpuError::LengthMismatch {
                expected: self.data.len(),
                actual: dst.len(),
            });
        }
        dst.clone_from_slice(self.data.as_slice());
        Ok(())
    }

    /// Explicitly downloads elements starting at `offset` into `dst`.
    ///
    /// Returns [`GpuError::LengthMismatch`] if `offset + dst.len() > self.len()`.
    pub fn download_range(&self, offset: usize, dst: &mut [T]) -> Result<(), GpuError>
    where
        T: Clone,
    {
        let Some(end) = offset.checked_add(dst.len()) else {
            return Err(GpuError::LengthMismatch {
                expected: self.data.len().saturating_sub(offset),
                actual: dst.len(),
            });
        };
        if end > self.data.len() {
            return Err(GpuError::LengthMismatch {
                expected: self.data.len().saturating_sub(offset),
                actual: dst.len(),
            });
        }
        dst.clone_from_slice(&self.data.as_slice()[offset..end]);
        Ok(())
    }

    /// Device-to-device copy from another [`DeviceBuffer`] of the same length.
    pub fn copy_from_device<B: Allocator>(
        &mut self,
        src: &DeviceBuffer<T, B>,
    ) -> Result<(), GpuError>
    where
        T: Clone,
    {
        self.upload_from(src.data.as_slice())
    }
}

impl<T, A: Allocator> fmt::Debug for DeviceBuffer<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceBuffer")
            .field("len", &self.data.len())
            .field("byte_len", &self._lease.bytes())
            .field("memory_kind", &self.memory_kind)
            .finish()
    }
}

/// 3D compute dispatch grid: `groups` workgroups, each with `threads` local invocations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grid {
    pub groups: [u32; 3],
    pub threads: [u32; 3],
}

impl Grid {
    /// Constructs a validated 3D compute dispatch grid.
    ///
    /// Rejects any zero dimension, any axis whose `groups[i] * threads[i]` overflows `u32`, or
    /// any grid whose total thread count overflows `u64`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn new(groups: [u32; 3], threads: [u32; 3]) -> Result<Grid, GpuError> {
        let grid = Grid { groups, threads };
        grid.validate()?;
        Ok(grid)
    }

    /// 1D grid covering at least `count` invocations with `threads_x` threads per workgroup.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn cover_1d(count: u32, threads_x: u32) -> Result<Grid, GpuError> {
        if count == 0 || threads_x == 0 {
            return Err(GpuError::InvalidGrid {
                groups: [0, 1, 1],
                threads: [threads_x, 1, 1],
            });
        }
        let groups_x = count.div_ceil(threads_x);
        Grid::new([groups_x, 1, 1], [threads_x, 1, 1])
    }

    /// 2D grid covering at least `width` × `height` invocations with `threads_x` × `threads_y`
    /// threads per workgroup.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn cover_2d(
        width: u32,
        height: u32,
        threads_x: u32,
        threads_y: u32,
    ) -> Result<Grid, GpuError> {
        if width == 0 || height == 0 || threads_x == 0 || threads_y == 0 {
            return Err(GpuError::InvalidGrid {
                groups: [0, 0, 1],
                threads: [threads_x, threads_y, 1],
            });
        }
        let groups_x = width.div_ceil(threads_x);
        let groups_y = height.div_ceil(threads_y);
        Grid::new([groups_x, groups_y, 1], [threads_x, threads_y, 1])
    }

    /// Validates that every dimension is non-zero and no global coordinate or total thread count
    /// overflows.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn validate(&self) -> Result<(), GpuError> {
        let err = || GpuError::InvalidGrid {
            groups: self.groups,
            threads: self.threads,
        };
        let mut total: u64 = 1;
        let mut axis = 0;
        while axis < 3 {
            let g = self.groups[axis];
            let t = self.threads[axis];
            if g == 0 || t == 0 {
                return Err(err());
            }
            let Some(extent) = g.checked_mul(t) else {
                return Err(err());
            };
            let Some(next) = total.checked_mul(u64::from(extent)) else {
                return Err(err());
            };
            total = next;
            axis += 1;
        }
        Ok(())
    }

    /// Total number of workgroups in the grid.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn total_groups(&self) -> u64 {
        u64::from(self.groups[0])
            .saturating_mul(u64::from(self.groups[1]))
            .saturating_mul(u64::from(self.groups[2]))
    }

    /// Number of local threads per workgroup.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn threads_per_group(&self) -> u64 {
        u64::from(self.threads[0])
            .saturating_mul(u64::from(self.threads[1]))
            .saturating_mul(u64::from(self.threads[2]))
    }

    /// Total number of thread invocations across all workgroups.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn total_threads(&self) -> u64 {
        self.total_groups().saturating_mul(self.threads_per_group())
    }
}

/// Coordinates of a single compute invocation inside a [`GpuScope::dispatch`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ThreadId {
    /// Workgroup coordinates `[gx, gy, gz]`.
    pub group: [u32; 3],
    /// Local thread coordinates `[lx, ly, lz]` within the workgroup.
    pub local: [u32; 3],
    /// Global thread coordinates `[gx * tx + lx, gy * ty + ly, gz * tz + lz]`.
    pub global: [u32; 3],
}

/// Execution summary returned by [`GpuScope::dispatch`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DispatchStats {
    /// Workgroups completed during this dispatch.
    pub groups_executed: u64,
    /// Total thread invocations completed during this dispatch.
    pub threads_executed: u64,
    /// Backend that executed the dispatch.
    pub backend: Backend,
}

/// Scoped GPU compute pass created by [`Gpu::scope`].
pub struct GpuScope<'g, 'cx> {
    gpu: &'g Gpu,
    cx: &'cx Cx,
    dispatches: u64,
    threads_executed: u64,
}

impl<'g, 'cx> GpuScope<'g, 'cx> {
    /// The [`Gpu`] capability backing this scope.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn gpu(&self) -> &'g Gpu {
        self.gpu
    }

    /// The task context governing cancellation and deadlines for this scope.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn cx(&self) -> &'cx Cx {
        self.cx
    }

    /// Number of dispatches completed so far in this scope.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn dispatches(&self) -> u64 {
        self.dispatches
    }

    /// Total thread invocations completed across all dispatches in this scope.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn total_threads_executed(&self) -> u64 {
        self.threads_executed
    }

    /// Marks `buf` synced for device execution inside this scope.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn sync_buffer<T, A: Allocator>(&mut self, buf: &mut Buffer<T, A>) {
        buf.mark_synced();
    }

    /// Dispatches `kernel` across `grid` deterministically.
    ///
    /// Workgroups execute in `(z, y, x)` order (`x` fastest) and local threads within each
    /// workgroup execute in `(z, y, x)` order. Cancellation and deadline checks against `cx`
    /// run before the dispatch and before every workgroup, aborting with
    /// [`GpuError::Cancelled`] or [`GpuError::Deadline`] if tripped.
    pub fn dispatch(
        &mut self,
        grid: Grid,
        mut kernel: impl FnMut(ThreadId),
    ) -> Result<DispatchStats, GpuError> {
        grid.validate()?;
        check_cx(self.cx)?;

        let [gx_max, gy_max, gz_max] = grid.groups;
        let [tx_max, ty_max, tz_max] = grid.threads;
        let per_group = grid.threads_per_group();

        let mut groups_executed: u64 = 0;
        let mut threads_executed: u64 = 0;

        for gz in 0..gz_max {
            let base_z = gz * tz_max;
            for gy in 0..gy_max {
                let base_y = gy * ty_max;
                for gx in 0..gx_max {
                    check_cx(self.cx)?;
                    let base_x = gx * tx_max;
                    for lz in 0..tz_max {
                        let global_z = base_z + lz;
                        for ly in 0..ty_max {
                            let global_y = base_y + ly;
                            for lx in 0..tx_max {
                                kernel(ThreadId {
                                    group: [gx, gy, gz],
                                    local: [lx, ly, lz],
                                    global: [base_x + lx, global_y, global_z],
                                });
                            }
                        }
                    }
                    groups_executed += 1;
                    threads_executed += per_group;
                }
            }
        }

        self.dispatches += 1;
        self.threads_executed += threads_executed;

        Ok(DispatchStats {
            groups_executed,
            threads_executed,
            backend: self.gpu.backend,
        })
    }
}

/// Offscreen RGBA8 pixel target allocated in a caller-provided allocator `A` and counted
/// against the [`Gpu`] capability's memory budget.
pub struct RenderTarget<A: Allocator> {
    width: u32,
    height: u32,
    pixels: OwnVec<u8, A>,
    memory_kind: MemoryKind,
    _lease: BudgetLease,
}

impl<A: Allocator> RenderTarget<A> {
    /// Width in pixels (always `>= 1`).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels (always `>= 1`).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Byte size (`width * height * 4`) charged against the [`Gpu`] budget.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn byte_len(&self) -> u64 {
        self._lease.bytes()
    }

    /// Memory coherence kind of the device that owns this target.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn memory_kind(&self) -> MemoryKind {
        self.memory_kind
    }

    /// Backing allocator handed in at construction.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn allocator(&self) -> &A {
        self.pixels.allocator()
    }

    /// Raw RGBA8 pixels in row-major order (`len == width * height * 4`).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn pixels(&self) -> &[u8] {
        self.pixels.as_slice()
    }

    /// Mutable raw RGBA8 pixels in row-major order (`len == width * height * 4`).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        self.pixels.as_mut_slice()
    }

    /// Fills every pixel with `rgba`.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn clear(&mut self, rgba: [u8; 4]) {
        for px in self.pixels.as_mut_slice().chunks_exact_mut(4) {
            px.copy_from_slice(&rgba);
        }
    }

    /// Writes `rgba` at `(x, y)`. Returns `true` if in bounds, `false` otherwise.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn set_pixel(&mut self, x: u32, y: u32, rgba: [u8; 4]) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let idx = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        self.pixels.as_mut_slice()[idx..idx + 4].copy_from_slice(&rgba);
        true
    }

    /// Reads the RGBA8 pixel at `(x, y)`, or `None` if out of bounds.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get_pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        let slice = self.pixels.as_slice();
        Some([slice[idx], slice[idx + 1], slice[idx + 2], slice[idx + 3]])
    }

    /// Evaluates `shader(x, y)` deterministically over every pixel in row-major order, checking
    /// `cx` for cancellation and deadline expiration before each scanline.
    pub fn fill_shader(
        &mut self,
        cx: &Cx,
        mut shader: impl FnMut(u32, u32) -> [u8; 4],
    ) -> Result<(), GpuError> {
        check_cx(cx)?;
        let width = self.width;
        let height = self.height;
        let row_bytes = (width as usize) * 4;
        let buf = self.pixels.as_mut_slice();
        for y in 0..height {
            check_cx(cx)?;
            let row_start = (y as usize) * row_bytes;
            let row = &mut buf[row_start..row_start + row_bytes];
            for x in 0..width {
                let offset = (x as usize) * 4;
                row[offset..offset + 4].copy_from_slice(&shader(x, y));
            }
        }
        Ok(())
    }
}

impl<A: Allocator> fmt::Debug for RenderTarget<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderTarget")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("byte_len", &self._lease.bytes())
            .field("memory_kind", &self.memory_kind)
            .finish()
    }
}
