//! Shared-memory telemetry event schema and the per-thread shadow return stack
//! (`#![no_std]`, `#![blaze::sans_io]`, zero allocations).
//!
//! High-frequency telemetry (function entry/exit spans, hook hits, allocations, I/O, waits,
//! samples) stays completely off the fd 3 CBOR control channel: the launcher writes fixed
//! 64-byte [`Event`] records into a lock-free shared-memory ring (`blaze-launch-rt`
//! `telemetry.rs`) that the parent or viewer drains out of band.
//!
//! # The shadow return stack ([`ShadowStack`])
//!
//! An entry+exit span hook (`blaze-launch-rt` `observe.rs`) replaces the callee's return
//! address (`x30` on AArch64) with `span_exit` without pushing a machine stack frame, so
//! stack-passed arguments at `[sp + 0]` reach the callee untouched. Each thread keeps a
//! bounded [`ShadowStack`] holding `(real_lr, caller_sp, slot, start_ns, span, parent)`:
//!
//! - **Downward stack invariant (AArch64 / x86-64):** stacks grow toward lower addresses.
//!   - A nested call has `caller_sp < top.caller_sp`.
//!   - A tail call between two hooked functions has `caller_sp == top.caller_sp`.
//!   - A frame skipped by an exception / `longjmp` / unwind has `top.caller_sp < caller_sp`
//!     (strict `<`), so [`ShadowStack::enter`] and [`ShadowStack::leave`] pop and report every
//!     entry with `entry.caller_sp < sp` as unwound without touching tail calls at `==`.
//! - **Bounded depth (rule 18):** at [`SHADOW_DEPTH`], [`ShadowStack::enter`] returns
//!   [`EnterOutcome::Overflow`] and increments [`ShadowStack::overflows`] without overwriting
//!   any saved return address; the trampoline leaves `x30` untouched so the call runs normally.
//! - **Panic unwinding:** [`ShadowStack::unwind_all`] drains every active entry top-to-bottom
//!   so `_Unwind_RaiseException` restores every saved `[fp + 8] == span_exit` slot on the
//!   machine stack to its `real_lr` before libunwind walks it.

use core::fmt;

use blaze_own::planted::{NoPlant, Planted, Plants};

use crate::Slot;

/// Environment variable carrying the shared-memory telemetry ring file path from `blaze run`
/// to `blaze-launcher`; stripped before the program's first instruction.
pub const VAR: &str = "BLAZE_LAUNCH_TELEMETRY";

/// Advisory lock file held shared (`LOCK_SH`) by any live telemetry viewer under `<runs_dir>`,
/// probed non-blocking (`LOCK_EX | LOCK_NB`) by `blaze run` / `blaze gc` to decide whether a dead
/// run's `.telem` and `.meta` files can be unlinked immediately without racing a live viewer.
pub const VIEWER_LOCK_LEAF: &str = "viewer.lock";

/// Header magic for a live run's shared-memory telemetry ring (not persisted across runs).
pub const MAGIC: [u8; 8] = *b"bztelem1";

/// Header format version for the live shared-memory telemetry ring.
/// Version 2 stores lap-relative turn counters (`seq - slot_index`), so zero-filled OS pages
/// are valid lap-0 slots without faulting the ring on `MappedRing::create`.
pub const VERSION: u32 = 2;

/// Maximum nested hooked spans per thread before overflowing to passthrough (rule 18).
pub const SHADOW_DEPTH: usize = 256;

/// Kind of a telemetry [`Event`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum EventKind {
    SpanEnter = 1,
    SpanExit = 2,
    SpanUnwound = 3,
    HookHit = 4,
    Alloc = 5,
    Free = 6,
    Io = 7,
    SyncWait = 8,
    ThreadBirth = 9,
    ThreadEnd = 10,
    Sample = 11,
}

impl EventKind {
    /// Decodes a raw byte from shared memory; unknown tags are refused (`None`).
    pub const fn from_raw(raw: u8) -> Option<EventKind> {
        match raw {
            1 => Some(EventKind::SpanEnter),
            2 => Some(EventKind::SpanExit),
            3 => Some(EventKind::SpanUnwound),
            4 => Some(EventKind::HookHit),
            5 => Some(EventKind::Alloc),
            6 => Some(EventKind::Free),
            7 => Some(EventKind::Io),
            8 => Some(EventKind::SyncWait),
            9 => Some(EventKind::ThreadBirth),
            10 => Some(EventKind::ThreadEnd),
            11 => Some(EventKind::Sample),
            _ => None,
        }
    }
}

/// Operation tag in [`Event::slot_or_op`] for [`EventKind::Alloc`] / [`EventKind::Free`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u32)]
pub enum AllocOp {
    Malloc = 1,
    Calloc = 2,
    Realloc = 3,
    Free = 4,
    PosixMemalign = 5,
    /// Rust `#[global_allocator]` or `std::alloc::System` `__rust_alloc` / `__rdl_alloc`.
    RustAlloc = 6,
    /// Rust `__rust_dealloc` / `__rdl_dealloc` (carries exact freed `size` in `b`).
    RustDealloc = 7,
    /// Rust `__rust_realloc` / `__rdl_realloc`.
    RustRealloc = 8,
    /// Rust `__rust_alloc_zeroed` / `__rdl_alloc_zeroed`.
    RustAllocZeroed = 9,
}

/// Which of the 4 Rust global-allocator entry slots an `AllocHook` stub wraps
/// (`__rust_alloc`, `__rust_dealloc`, `__rust_realloc`, `__rust_alloc_zeroed`, or `__rdl_*`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum AllocHookKind {
    Alloc = 1,
    Dealloc = 2,
    Realloc = 3,
    AllocZeroed = 4,
}

impl AllocHookKind {
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Alloc),
            2 => Some(Self::Dealloc),
            3 => Some(Self::Realloc),
            4 => Some(Self::AllocZeroed),
            _ => None,
        }
    }

    pub const fn alloc_op(self) -> AllocOp {
        match self {
            Self::Alloc => AllocOp::RustAlloc,
            Self::Dealloc => AllocOp::RustDealloc,
            Self::Realloc => AllocOp::RustRealloc,
            Self::AllocZeroed => AllocOp::RustAllocZeroed,
        }
    }
}

/// Operation tag in [`Event::slot_or_op`] for [`EventKind::Io`] / [`EventKind::SyncWait`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u32)]
pub enum IoOp {
    Open = 1,
    Close = 2,
    Read = 3,
    Write = 4,
    Connect = 5,
    Getenv = 6,
    MutexLock = 7,
}

/// Unique per-thread span identifier (`SpanId(0)` means no parent span).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct SpanId(pub u64);

impl SpanId {
    pub const NONE: SpanId = SpanId(0);
}

/// Sentinel value for [`Event::cpu`] when the OS did not report a CPU core.
pub const CPU_UNKNOWN: u16 = u16::MAX;

/// One fixed 64-byte telemetry record in the shared-memory ring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C, align(8))]
pub struct Event {
    pub kind: u8,
    /// Low 4 bits: NUMA node / cluster (`0..15`); bit 4: efficiency core (`1 = E-core`).
    pub flags: u8,
    /// Hardware CPU core index (`0..65534`, or [`CPU_UNKNOWN`]).
    pub cpu: u16,
    pub thread: u32,
    pub slot_or_op: u32,
    pub _pad2: u32,
    pub ts_ns: u64,
    pub a: u64,
    pub b: u64,
    pub c: u64,
    pub d: u64,
    pub e: u64,
}

const _: () = assert!(core::mem::size_of::<Event>() == 64);
const _: () = assert!(core::mem::align_of::<Event>() == 8);

impl Event {
    /// Zeroed placeholder for empty ring slots.
    pub const EMPTY: Event = Event {
        kind: 0,
        flags: 0,
        cpu: CPU_UNKNOWN,
        thread: 0,
        slot_or_op: 0,
        _pad2: 0,
        ts_ns: 0,
        a: 0,
        b: 0,
        c: 0,
        d: 0,
        e: 0,
    };

    #[inline]
    pub const fn with_cpu(mut self, cpu: u16, flags: u8) -> Event {
        self.cpu = cpu;
        self.flags = flags;
        self
    }

    #[inline]
    pub const fn numa_node(self) -> u8 {
        self.flags & 0x0f
    }

    #[inline]
    pub const fn is_e_core(self) -> bool {
        (self.flags & 0x10) != 0
    }

    /// Typed kind, or `None` if the tag is invalid/empty.
    pub const fn event_kind(&self) -> Option<EventKind> {
        EventKind::from_raw(self.kind)
    }

    const fn slot_event(
        kind: EventKind,
        thread: u32,
        slot: Slot,
        ts_ns: u64,
        a: u64,
        span: SpanId,
        parent: SpanId,
    ) -> Event {
        Event {
            kind: kind as u8,
            flags: 0,
            cpu: CPU_UNKNOWN,
            thread,
            slot_or_op: slot.0,
            _pad2: 0,
            ts_ns,
            a,
            b: span.0,
            c: parent.0,
            d: 0,
            e: 0,
        }
    }

    pub const fn span_enter(
        thread: u32,
        slot: Slot,
        ts_ns: u64,
        caller_lr: u64,
        span: SpanId,
        parent: SpanId,
    ) -> Event {
        Self::slot_event(EventKind::SpanEnter, thread, slot, ts_ns, caller_lr, span, parent)
    }

    pub const fn span_exit(
        thread: u32,
        slot: Slot,
        ts_ns: u64,
        duration_ns: u64,
        span: SpanId,
        parent: SpanId,
    ) -> Event {
        Self::slot_event(EventKind::SpanExit, thread, slot, ts_ns, duration_ns, span, parent)
    }

    pub const fn span_unwound(
        thread: u32,
        slot: Slot,
        ts_ns: u64,
        duration_ns: u64,
        span: SpanId,
        parent: SpanId,
    ) -> Event {
        Self::slot_event(EventKind::SpanUnwound, thread, slot, ts_ns, duration_ns, span, parent)
    }

    pub const fn hook_hit(thread: u32, slot: Slot, ts_ns: u64, caller_lr: u64) -> Event {
        Self::slot_event(EventKind::HookHit, thread, slot, ts_ns, caller_lr, SpanId::NONE, SpanId::NONE)
    }

    pub const fn alloc(
        thread: u32,
        op: AllocOp,
        ts_ns: u64,
        ptr: u64,
        bytes: u64,
        caller_frames: [u64; 3],
    ) -> Event {
        let kind = match op {
            AllocOp::Free | AllocOp::RustDealloc => EventKind::Free as u8,
            AllocOp::Malloc
            | AllocOp::Calloc
            | AllocOp::Realloc
            | AllocOp::PosixMemalign
            | AllocOp::RustAlloc
            | AllocOp::RustRealloc
            | AllocOp::RustAllocZeroed => EventKind::Alloc as u8,
        };
        Event {
            kind,
            flags: 0,
            cpu: CPU_UNKNOWN,
            thread,
            slot_or_op: op as u32,
            _pad2: 0,
            ts_ns,
            a: ptr,
            b: bytes,
            c: caller_frames[0],
            d: caller_frames[1],
            e: caller_frames[2],
        }
    }

    pub const fn io(
        thread: u32,
        op: IoOp,
        ts_ns: u64,
        duration_ns: u64,
        fd_or_arg: u64,
        result: u64,
        caller_lr: u64,
    ) -> Event {
        let kind = match op {
            IoOp::MutexLock => EventKind::SyncWait as u8,
            _ => EventKind::Io as u8,
        };
        Event {
            kind,
            flags: 0,
            cpu: CPU_UNKNOWN,
            thread,
            slot_or_op: op as u32,
            _pad2: 0,
            ts_ns,
            a: duration_ns,
            b: fd_or_arg,
            c: result,
            d: caller_lr,
            e: 0,
        }
    }

    pub const fn thread_lifecycle(thread: u32, birth: bool, ts_ns: u64) -> Event {
        Event {
            kind: if birth { EventKind::ThreadBirth as u8 } else { EventKind::ThreadEnd as u8 },
            flags: 0,
            cpu: CPU_UNKNOWN,
            thread,
            slot_or_op: 0,
            _pad2: 0,
            ts_ns,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
            e: 0,
        }
    }

    /// Thread creation event attributing the spawning parent thread and 3-frame caller stack.
    pub const fn thread_birth(
        thread: u32,
        ts_ns: u64,
        parent_thread: u32,
        spawn_frames: [u64; 3],
    ) -> Event {
        Event {
            kind: EventKind::ThreadBirth as u8,
            flags: 0,
            cpu: CPU_UNKNOWN,
            thread,
            slot_or_op: 0,
            _pad2: 0,
            ts_ns,
            a: parent_thread as u64,
            b: 0,
            c: spawn_frames[0],
            d: spawn_frames[1],
            e: spawn_frames[2],
        }
    }

    /// Mutex / lock wait event attributing both the blocked waiter (`c`) and the blocking
    /// lock holder (`d` = holder thread ID, `e` = holder acquire caller PC).
    pub const fn sync_wait(
        thread: u32,
        ts_ns: u64,
        wait_ns: u64,
        mutex_ptr: u64,
        waiter_pc: u64,
        holder_thread: u32,
        holder_pc: u64,
    ) -> Event {
        Event {
            kind: EventKind::SyncWait as u8,
            flags: 0,
            cpu: CPU_UNKNOWN,
            thread,
            slot_or_op: IoOp::MutexLock as u32,
            _pad2: 0,
            ts_ns,
            a: wait_ns,
            b: mutex_ptr,
            c: waiter_pc,
            d: holder_thread as u64,
            e: holder_pc,
        }
    }
}

/// One frame saved on a thread's [`ShadowStack`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShadowEntry {
    pub slot: Slot,
    pub real_lr: u64,
    pub caller_sp: u64,
    pub start_ns: u64,
    pub span: SpanId,
    pub parent: SpanId,
}

impl ShadowEntry {
    const EMPTY: ShadowEntry = ShadowEntry {
        slot: Slot(0),
        real_lr: 0,
        caller_sp: 0,
        start_ns: 0,
        span: SpanId::NONE,
        parent: SpanId::NONE,
    };
}

/// What [`ShadowStack::enter`] decided for the calling trampoline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnterOutcome {
    /// Entry was pushed; the trampoline should replace `x30` with `span_exit`.
    Hooked { span: SpanId, parent: SpanId },
    /// The shadow stack is at [`SHADOW_DEPTH`]; the trampoline must leave `x30` untouched.
    Overflow,
}

/// One span popped because its frame had already unwound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnwoundSpan {
    pub slot: Slot,
    pub real_lr: u64,
    pub caller_sp: u64,
    pub span: SpanId,
    pub parent: SpanId,
    pub duration_ns: u64,
}

/// What [`ShadowStack::leave`] popped on a normal return.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LeftSpan {
    pub slot: Slot,
    pub real_lr: u64,
    pub span: SpanId,
    pub parent: SpanId,
    pub duration_ns: u64,
}

/// Why [`ShadowStack::leave`] could not pop a return address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShadowError {
    /// `span_exit` ran with no matching frame on the shadow stack.
    Empty { current_sp: u64 },
}

impl fmt::Display for ShadowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ShadowError::Empty { current_sp } => {
                write!(f, "span_exit reached at sp {current_sp:#x} with an empty shadow stack")
            }
        }
    }
}

impl core::error::Error for ShadowError {}

/// Planted-bug controls for the [`ShadowStack`] simulation (rule 43 / 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShadowPlant {
    None,
    /// Heals entries with `entry.caller_sp <= sp` instead of `<`, corrupting tail calls.
    HealOnEqualSp,
    /// Overwrites the top entry when full instead of returning [`EnterOutcome::Overflow`].
    OverwriteOnFull,
    /// Fails to increment `next_span`, issuing duplicate span IDs.
    ReuseSpanId,
}

impl Plants for ShadowPlant {}

/// Per-thread bounded shadow return stack. Zero heap allocations; all logic sans-IO.
pub struct ShadowStack<P: Planted<ShadowPlant> = NoPlant> {
    entries: [ShadowEntry; SHADOW_DEPTH],
    len: usize,
    next_span: u64,
    overflows: u64,
    unwound: u64,
    plant: P,
}

impl ShadowStack<NoPlant> {
    /// An empty shadow stack starting span IDs at `1`.
    pub const fn new() -> ShadowStack<NoPlant> {
        ShadowStack::planted(NoPlant)
    }
}

impl Default for ShadowStack<NoPlant> {
    fn default() -> ShadowStack<NoPlant> {
        ShadowStack::new()
    }
}

impl<P: Planted<ShadowPlant>> ShadowStack<P> {
    pub const fn planted(plant: P) -> ShadowStack<P> {
        ShadowStack {
            entries: [ShadowEntry::EMPTY; SHADOW_DEPTH],
            len: 0,
            next_span: 1,
            overflows: 0,
            unwound: 0,
            plant,
        }
    }

    pub const fn depth(&self) -> usize {
        self.len
    }

    pub const fn overflows(&self) -> u64 {
        self.overflows
    }

    pub const fn unwound(&self) -> u64 {
        self.unwound
    }

    pub fn current_span(&self) -> SpanId {
        match self.len.checked_sub(1) {
            Some(top) => self.entries[top].span,
            None => SpanId::NONE,
        }
    }

    /// Pops any top entries strictly below `sp` on a downward-growing stack (frames skipped by
    /// an unwind), invoking `on_unwound` for each.
    fn heal(&mut self, sp: u64, ts_ns: u64, on_unwound: &mut impl FnMut(UnwoundSpan)) {
        let heal_equal = self.plant.has(ShadowPlant::HealOnEqualSp);
        while let Some(top) = self.len.checked_sub(1) {
            let entry = self.entries[top];
            let stale = if heal_equal { entry.caller_sp <= sp } else { entry.caller_sp < sp };
            if !stale {
                break;
            }
            self.len = top;
            self.unwound = self.unwound.saturating_add(1);
            on_unwound(UnwoundSpan {
                slot: entry.slot,
                real_lr: entry.real_lr,
                caller_sp: entry.caller_sp,
                span: entry.span,
                parent: entry.parent,
                duration_ns: ts_ns.saturating_sub(entry.start_ns),
            });
        }
    }

    /// Records a hooked function entry at `caller_sp`, healing any stale unwound frames first.
    pub fn enter(
        &mut self,
        slot: Slot,
        real_lr: u64,
        caller_sp: u64,
        ts_ns: u64,
        mut on_unwound: impl FnMut(UnwoundSpan),
    ) -> EnterOutcome {
        self.heal(caller_sp, ts_ns, &mut on_unwound);
        let parent = self.current_span();
        if self.len >= SHADOW_DEPTH {
            self.overflows = self.overflows.saturating_add(1);
            if self.plant.has(ShadowPlant::OverwriteOnFull) {
                let span = SpanId(self.next_span);
                if !self.plant.has(ShadowPlant::ReuseSpanId) {
                    self.next_span = self.next_span.saturating_add(1);
                }
                self.entries[SHADOW_DEPTH - 1] =
                    ShadowEntry { slot, real_lr, caller_sp, start_ns: ts_ns, span, parent };
                return EnterOutcome::Hooked { span, parent };
            }
            return EnterOutcome::Overflow;
        }
        let span = SpanId(self.next_span);
        if !self.plant.has(ShadowPlant::ReuseSpanId) {
            self.next_span = self.next_span.saturating_add(1);
        }
        self.entries[self.len] =
            ShadowEntry { slot, real_lr, caller_sp, start_ns: ts_ns, span, parent };
        self.len += 1;
        EnterOutcome::Hooked { span, parent }
    }

    /// Pops the returning frame at `current_sp` and hands back its `real_lr`.
    pub fn leave(
        &mut self,
        current_sp: u64,
        ts_ns: u64,
        mut on_unwound: impl FnMut(UnwoundSpan),
    ) -> Result<LeftSpan, ShadowError> {
        self.heal(current_sp, ts_ns, &mut on_unwound);
        let Some(top) = self.len.checked_sub(1) else {
            return Err(ShadowError::Empty { current_sp });
        };
        let entry = self.entries[top];
        self.len = top;
        Ok(LeftSpan {
            slot: entry.slot,
            real_lr: entry.real_lr,
            span: entry.span,
            parent: entry.parent,
            duration_ns: ts_ns.saturating_sub(entry.start_ns),
        })
    }

    /// Drains every active entry top-to-bottom when unwinding begins (`_Unwind_RaiseException`),
    /// so the driver can restore every `span_exit` saved on the machine stack back to `real_lr`.
    pub fn unwind_all(&mut self, ts_ns: u64, mut on_unwound: impl FnMut(UnwoundSpan)) {
        while let Some(top) = self.len.checked_sub(1) {
            let entry = self.entries[top];
            self.len = top;
            self.unwound = self.unwound.saturating_add(1);
            on_unwound(UnwoundSpan {
                slot: entry.slot,
                real_lr: entry.real_lr,
                caller_sp: entry.caller_sp,
                span: entry.span,
                parent: entry.parent,
                duration_ns: ts_ns.saturating_sub(entry.start_ns),
            });
        }
    }
}
