//! High-performance Wasmtime 64 (`memory64`) sans-IO ring ABI for `bstd`.
//!
//! Instead of paying one Wasm↔Host call per POSIX/WASI syscall, `bstd::wasm64` batches `N`
//! sans-IO effects (`SqEntry`, 64 bytes each, 64-bit guest linear-memory pointers) into one
//! submission ring and crosses into the Wasmtime 64 host provider (`module = "blaze_host_v1"`)
//! once per batch via `submit_wait(ring_ptr: i64, submitted: i32, min_complete: i32, timeout_ns: i64)`.
//!
//! The host provider negotiates its granted [`HostCaps`] once up front; any ungranted capability
//! is refused deterministically by name ([`RingError::MissingCapability`]), with zero fallback.
//!
//! `#![no_std]`, `#![forbid(unsafe_code)]`, explicit allocator only.

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

/// Canonical Wasm64 host import module name (`blaze-export::wasm64::HOST_MODULE`).
pub const HOST_MODULE: &str = "blaze_host_v1";
/// Sans-IO ring ABI version (matches `blaze_ring_abi_version` global export).
pub const ABI_VERSION: u32 = 1;

/// Capabilities granted by a Wasmtime 64 host provider (negotiated once via `host_caps`).
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash, Debug)]
pub struct HostCaps(pub u64);

impl HostCaps {
    pub const NONE: HostCaps = HostCaps(0);
    pub const FS_READ: HostCaps = HostCaps(1 << 0);
    pub const FS_WRITE: HostCaps = HostCaps(1 << 1);
    pub const CLOCK: HostCaps = HostCaps(1 << 2);
    pub const RANDOM: HostCaps = HostCaps(1 << 3);
    pub const HTTP: HostCaps = HostCaps(1 << 4);
    pub const KV: HostCaps = HostCaps(1 << 5);
    pub const STDIO: HostCaps = HostCaps(1 << 6);
    pub const GPU: HostCaps = HostCaps(1 << 7);
    pub const ALL: HostCaps = HostCaps((1 << 8) - 1);

    #[inline]
    pub const fn union(self, other: HostCaps) -> HostCaps {
        HostCaps(self.0 | other.0)
    }

    #[inline]
    pub const fn has(self, need: HostCaps) -> bool {
        (self.0 & need.0) == need.0
    }
}

/// Sans-IO effect opcode transmitted in a 64-byte [`SqEntry`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum OpCode {
    Nop = 0,
    FsRead = 1,
    FsWrite = 2,
    FsStat = 3,
    FsEntries = 4,
    FsPublish = 5,
    FsRemove = 6,
    FsSyncDir = 7,
    ClockWall = 8,
    ClockMonotonic = 9,
    RandomBytes = 10,
    HttpFetch = 11,
    KvGet = 12,
    KvPut = 13,
    StdoutWrite = 14,
    StdinRead = 15,
    GpuSubmit = 16,
}

impl OpCode {
    /// The host capability required to execute this opcode.
    #[inline]
    pub const fn required_cap(self) -> HostCaps {
        match self {
            OpCode::Nop => HostCaps::NONE,
            OpCode::FsRead | OpCode::FsStat | OpCode::FsEntries => HostCaps::FS_READ,
            OpCode::FsWrite | OpCode::FsPublish | OpCode::FsRemove | OpCode::FsSyncDir => {
                HostCaps::FS_WRITE
            }
            OpCode::ClockWall | OpCode::ClockMonotonic => HostCaps::CLOCK,
            OpCode::RandomBytes => HostCaps::RANDOM,
            OpCode::HttpFetch => HostCaps::HTTP,
            OpCode::KvGet | OpCode::KvPut => HostCaps::KV,
            OpCode::StdoutWrite | OpCode::StdinRead => HostCaps::STDIO,
            OpCode::GpuSubmit => HostCaps::GPU,
        }
    }

    #[inline]
    pub const fn from_u16(raw: u16) -> Option<OpCode> {
        match raw {
            0 => Some(OpCode::Nop),
            1 => Some(OpCode::FsRead),
            2 => Some(OpCode::FsWrite),
            3 => Some(OpCode::FsStat),
            4 => Some(OpCode::FsEntries),
            5 => Some(OpCode::FsPublish),
            6 => Some(OpCode::FsRemove),
            7 => Some(OpCode::FsSyncDir),
            8 => Some(OpCode::ClockWall),
            9 => Some(OpCode::ClockMonotonic),
            10 => Some(OpCode::RandomBytes),
            11 => Some(OpCode::HttpFetch),
            12 => Some(OpCode::KvGet),
            13 => Some(OpCode::KvPut),
            14 => Some(OpCode::StdoutWrite),
            15 => Some(OpCode::StdinRead),
            16 => Some(OpCode::GpuSubmit),
            _ => None,
        }
    }
}

/// One 64-byte (`#[repr(C, align(64))]`) submission entry using native 64-bit Wasm64 pointers.
#[repr(C, align(64))]
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SqEntry {
    /// Caller-chosen correlation tag echoed back verbatim in [`CqEntry::tag`].
    pub tag: u64,
    /// Effect [`OpCode`] as `u16`.
    pub opcode: u16,
    /// Per-op flags (e.g. `Durable::Yes` = bit 0).
    pub flags: u16,
    /// Capability handle (e.g. directory or KV namespace id).
    pub cap_handle: u32,
    /// Primary 64-bit linear-memory buffer pointer (e.g. output read buffer or payload).
    pub ptr: u64,
    /// Primary buffer length in bytes.
    pub len: u64,
    /// Secondary 64-bit linear-memory pointer (e.g. relative path or KV key).
    pub aux_ptr: u64,
    /// Secondary buffer length in bytes.
    pub aux_len: u64,
    /// Positional file offset or op-specific parameter.
    pub offset: u64,
    /// Per-op deadline in nanoseconds (`0` = no deadline).
    pub deadline_ns: u64,
}

/// Completion status codes returned in [`CqEntry::status`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum RingStatus {
    Ok = 0,
    NotFound = -1,
    PermissionDenied = -2,
    TimedOut = -3,
    InvalidArg = -4,
    BufferTooSmall = -5,
    IoFailed = -6,
    Unsupported = -7,
}

impl RingStatus {
    #[inline]
    pub const fn from_i32(raw: i32) -> RingStatus {
        match raw {
            0 => RingStatus::Ok,
            -1 => RingStatus::NotFound,
            -2 => RingStatus::PermissionDenied,
            -3 => RingStatus::TimedOut,
            -4 => RingStatus::InvalidArg,
            -5 => RingStatus::BufferTooSmall,
            -6 => RingStatus::IoFailed,
            _ => RingStatus::Unsupported,
        }
    }
}

/// One 32-byte (`#[repr(C, align(32))]`) completion entry written by the Wasmtime 64 host.
#[repr(C, align(32))]
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct CqEntry {
    pub tag: u64,
    pub status: i32,
    pub flags: u32,
    /// Primary result (e.g. bytes read/written, wall/monotonic ns, HTTP status code).
    pub result_u64: u64,
    /// Secondary result (e.g. total file length, content digest prefix).
    pub aux_u64: u64,
}

/// Typed refusal from [`Wasm64Ring`] or [`WasmHostProvider`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RingError {
    RingFull { capacity: usize },
    MissingCapability { opcode: OpCode, required: HostCaps },
    AbiMismatch { expected: u32, got: u32 },
    InvalidSlice { ptr: u64, len: u64 },
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RingError::RingFull { capacity } => {
                write!(f, "bstd::wasm64 ring is full (capacity {capacity}): flush before submitting more ops")
            }
            RingError::MissingCapability { opcode, required } => {
                write!(f, "bstd::wasm64 host refused {opcode:?}: missing capability {:?}", required)
            }
            RingError::AbiMismatch { expected, got } => {
                write!(f, "bstd::wasm64 host ABI version mismatch: expected {expected}, got {got}")
            }
            RingError::InvalidSlice { ptr, len } => {
                write!(f, "bstd::wasm64 invalid 64-bit guest memory slice [{ptr}..+{len}]")
            }
        }
    }
}

impl core::error::Error for RingError {}

/// Trait implemented by any Wasmtime 64 host provider (native `io_uring`/kqueue host or
/// deterministic simulation host) serving Blaze's sans-IO ring ABI.
pub trait WasmHostProvider {
    /// Returns the host's ABI version (must match [`ABI_VERSION`]).
    fn abi_version(&self) -> u32 {
        ABI_VERSION
    }

    /// Negotiates the set of capabilities granted to this Wasm64 guest.
    fn caps(&self) -> HostCaps;

    /// Executes a batch of submitted [`SqEntry`] descriptors in a single host boundary crossing,
    /// reading/writing guest buffers in `guest_memory`, writing completions into `cq_out`, and
    /// returning how many completions were posted.
    fn submit_wait(
        &mut self,
        sq: &[SqEntry],
        cq_out: &mut [CqEntry],
        guest_memory: &mut [u8],
        min_complete: u32,
        timeout_ns: u64,
    ) -> Result<u32, RingError>;
}

/// Batched sans-IO submission/completion ring for Wasmtime 64 guests in an explicit allocator.
pub struct Wasm64Ring<A: Allocator> {
    caps: HostCaps,
    sq: Vec<SqEntry, A>,
    cq: Vec<CqEntry, A>,
    cq_head: usize,
    capacity: usize,
    next_tag: u64,
    host_crossings: u64,
}

impl<A: Allocator + Clone> Wasm64Ring<A> {
    /// Creates a ring sized for `capacity` in-flight operations, verifying `provider`'s ABI version
    /// and caching its granted [`HostCaps`].
    pub fn new_in(
        provider: &dyn WasmHostProvider,
        capacity: usize,
        alloc: A,
    ) -> Result<Wasm64Ring<A>, RingError> {
        let got = provider.abi_version();
        if got != ABI_VERSION {
            return Err(RingError::AbiMismatch { expected: ABI_VERSION, got });
        }
        let cap = capacity.max(1);
        Ok(Wasm64Ring {
            caps: provider.caps(),
            sq: Vec::with_capacity_in(cap, alloc.clone()),
            cq: Vec::with_capacity_in(cap, alloc),
            cq_head: 0,
            capacity: cap,
            next_tag: 1,
            host_crossings: 0,
        })
    }

    #[inline]
    pub fn caps(&self) -> HostCaps {
        self.caps
    }

    #[inline]
    pub fn pending_submissions(&self) -> usize {
        self.sq.len()
    }

    #[inline]
    pub fn ready_completions(&self) -> usize {
        self.cq.len().saturating_sub(self.cq_head)
    }

    /// Total guest→host boundary crossings made so far (proves batch amortization).
    #[inline]
    pub fn host_crossings(&self) -> u64 {
        self.host_crossings
    }

    /// Enqueues one [`SqEntry`] into the submission ring without crossing into the host.
    /// Refuses immediately if the host did not grant the opcode's required capability.
    pub fn push(&mut self, opcode: OpCode, mut entry: SqEntry) -> Result<u64, RingError> {
        let need = opcode.required_cap();
        if !self.caps.has(need) {
            return Err(RingError::MissingCapability { opcode, required: need });
        }
        if self.sq.len() >= self.capacity {
            return Err(RingError::RingFull { capacity: self.capacity });
        }
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        entry.tag = tag;
        entry.opcode = opcode as u16;
        self.sq.push(entry);
        Ok(tag)
    }

    /// Convenience helper: queues a `ClockMonotonic` read.
    pub fn queue_clock_monotonic(&mut self) -> Result<u64, RingError> {
        self.push(OpCode::ClockMonotonic, SqEntry::default())
    }

    /// Convenience helper: queues a `RandomBytes` draw directly into guest linear memory `[ptr..+len]`.
    pub fn queue_random(&mut self, ptr: u64, len: u64) -> Result<u64, RingError> {
        self.push(OpCode::RandomBytes, SqEntry { ptr, len, ..SqEntry::default() })
    }

    /// Convenience helper: queues a `StdoutWrite` from guest linear memory `[ptr..+len]`.
    pub fn queue_stdout(&mut self, ptr: u64, len: u64) -> Result<u64, RingError> {
        self.push(OpCode::StdoutWrite, SqEntry { ptr, len, ..SqEntry::default() })
    }

    /// Convenience helper: queues a `KvGet` (key at `[key_ptr..+key_len]`, value written to `[out_ptr..+out_len]`).
    pub fn queue_kv_get(&mut self, key_ptr: u64, key_len: u64, out_ptr: u64, out_len: u64) -> Result<u64, RingError> {
        self.push(
            OpCode::KvGet,
            SqEntry { ptr: out_ptr, len: out_len, aux_ptr: key_ptr, aux_len: key_len, ..SqEntry::default() },
        )
    }

    /// Convenience helper: queues a `KvPut` (key at `[key_ptr..+key_len]`, value at `[val_ptr..+val_len]`).
    pub fn queue_kv_put(&mut self, key_ptr: u64, key_len: u64, val_ptr: u64, val_len: u64) -> Result<u64, RingError> {
        self.push(
            OpCode::KvPut,
            SqEntry { ptr: val_ptr, len: val_len, aux_ptr: key_ptr, aux_len: key_len, ..SqEntry::default() },
        )
    }

    /// Submits ALL queued [`SqEntry`] operations in **one single host crossing**, waiting for at
    /// least `min_complete` completions within `timeout_ns`, and returns the number of new
    /// completions received.
    pub fn flush_wait(
        &mut self,
        provider: &mut dyn WasmHostProvider,
        guest_memory: &mut [u8],
        min_complete: u32,
        timeout_ns: u64,
    ) -> Result<u32, RingError> {
        if self.sq.is_empty() && min_complete == 0 {
            return Ok(0);
        }
        if self.cq_head >= self.cq.len() {
            self.cq.clear();
            self.cq_head = 0;
        }
        let prev_len = self.cq.len();
        let need_slots = self.sq.len().max(min_complete as usize);
        self.cq.resize(prev_len + need_slots, CqEntry::default());
        let posted = provider.submit_wait(
            &self.sq,
            &mut self.cq[prev_len..],
            guest_memory,
            min_complete,
            timeout_ns,
        )?;
        self.cq.truncate(prev_len + posted as usize);
        self.sq.clear();
        self.host_crossings += 1;
        Ok(posted)
    }

    /// Pops the next completed [`CqEntry`], or `None` if all completions have been drained.
    #[inline]
    pub fn pop_completion(&mut self) -> Option<CqEntry> {
        if self.cq_head < self.cq.len() {
            let entry = self.cq[self.cq_head];
            self.cq_head += 1;
            if self.cq_head == self.cq.len() {
                self.cq.clear();
                self.cq_head = 0;
            }
            Some(entry)
        } else {
            None
        }
    }
}

/// Deterministic in-memory Wasmtime 64 host provider for tests, simulation, and reference behavior.
pub struct SimWasmHost<A: Allocator> {
    caps: HostCaps,
    monotonic_ns: u64,
    wall_ns: u64,
    rng_state: u64,
    stdout: Vec<u8, A>,
    kv: Vec<(Vec<u8, A>, Vec<u8, A>), A>,
    alloc: A,
}

impl<A: Allocator + Clone> SimWasmHost<A> {
    pub fn new_in(caps: HostCaps, seed: u64, alloc: A) -> SimWasmHost<A> {
        SimWasmHost {
            caps,
            monotonic_ns: 1_000_000,
            wall_ns: 1_700_000_000_000_000_000,
            rng_state: seed ^ 0x9E37_79B9_7F4A_7C15,
            stdout: Vec::new_in(alloc.clone()),
            kv: Vec::new_in(alloc.clone()),
            alloc,
        }
    }

    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    fn next_rand_u64(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        x
    }
}

fn slice_range(mem_len: usize, ptr: u64, len: u64) -> Result<core::ops::Range<usize>, RingError> {
    let start = usize::try_from(ptr).map_err(|_| RingError::InvalidSlice { ptr, len })?;
    let count = usize::try_from(len).map_err(|_| RingError::InvalidSlice { ptr, len })?;
    let end = start.checked_add(count).ok_or(RingError::InvalidSlice { ptr, len })?;
    if end > mem_len {
        return Err(RingError::InvalidSlice { ptr, len });
    }
    Ok(start..end)
}

impl<A: Allocator + Clone> WasmHostProvider for SimWasmHost<A> {
    fn caps(&self) -> HostCaps {
        self.caps
    }

    fn submit_wait(
        &mut self,
        sq: &[SqEntry],
        cq_out: &mut [CqEntry],
        guest_memory: &mut [u8],
        _min_complete: u32,
        _timeout_ns: u64,
    ) -> Result<u32, RingError> {
        let mut posted = 0usize;
        for entry in sq {
            if posted >= cq_out.len() {
                break;
            }
            let Some(op) = OpCode::from_u16(entry.opcode) else {
                cq_out[posted] = CqEntry {
                    tag: entry.tag,
                    status: RingStatus::Unsupported as i32,
                    ..CqEntry::default()
                };
                posted += 1;
                continue;
            };
            if !self.caps.has(op.required_cap()) {
                cq_out[posted] = CqEntry {
                    tag: entry.tag,
                    status: RingStatus::PermissionDenied as i32,
                    ..CqEntry::default()
                };
                posted += 1;
                continue;
            }
            let cq = match op {
                OpCode::Nop => CqEntry { tag: entry.tag, status: RingStatus::Ok as i32, ..CqEntry::default() },
                OpCode::ClockMonotonic => {
                    self.monotonic_ns += 100;
                    CqEntry {
                        tag: entry.tag,
                        status: RingStatus::Ok as i32,
                        result_u64: self.monotonic_ns,
                        ..CqEntry::default()
                    }
                }
                OpCode::ClockWall => CqEntry {
                    tag: entry.tag,
                    status: RingStatus::Ok as i32,
                    result_u64: self.wall_ns,
                    ..CqEntry::default()
                },
                OpCode::RandomBytes => {
                    let range = slice_range(guest_memory.len(), entry.ptr, entry.len)?;
                    let dst = &mut guest_memory[range];
                    let mut i = 0;
                    while i < dst.len() {
                        let word = self.next_rand_u64().to_le_bytes();
                        let take = (dst.len() - i).min(8);
                        dst[i..i + take].copy_from_slice(&word[..take]);
                        i += take;
                    }
                    CqEntry {
                        tag: entry.tag,
                        status: RingStatus::Ok as i32,
                        result_u64: entry.len,
                        ..CqEntry::default()
                    }
                }
                OpCode::StdoutWrite => {
                    let range = slice_range(guest_memory.len(), entry.ptr, entry.len)?;
                    self.stdout.extend_from_slice(&guest_memory[range]);
                    CqEntry {
                        tag: entry.tag,
                        status: RingStatus::Ok as i32,
                        result_u64: entry.len,
                        ..CqEntry::default()
                    }
                }
                OpCode::KvPut => {
                    let krange = slice_range(guest_memory.len(), entry.aux_ptr, entry.aux_len)?;
                    let vrange = slice_range(guest_memory.len(), entry.ptr, entry.len)?;
                    let key_slice = &guest_memory[krange];
                    let val_slice = &guest_memory[vrange];
                    if let Some((_, existing)) = self.kv.iter_mut().find(|(k, _)| k.as_slice() == key_slice) {
                        existing.clear();
                        existing.extend_from_slice(val_slice);
                    } else {
                        let mut k = Vec::with_capacity_in(key_slice.len(), self.alloc.clone());
                        k.extend_from_slice(key_slice);
                        let mut v = Vec::with_capacity_in(val_slice.len(), self.alloc.clone());
                        v.extend_from_slice(val_slice);
                        self.kv.push((k, v));
                    }
                    CqEntry {
                        tag: entry.tag,
                        status: RingStatus::Ok as i32,
                        result_u64: entry.len,
                        ..CqEntry::default()
                    }
                }
                OpCode::KvGet => {
                    let krange = slice_range(guest_memory.len(), entry.aux_ptr, entry.aux_len)?;
                    let out_range = slice_range(guest_memory.len(), entry.ptr, entry.len)?;
                    let found_val = {
                        let key_slice = &guest_memory[krange];
                        self.kv.iter().find(|(k, _)| k.as_slice() == key_slice).map(|(_, v)| v.as_slice())
                    };
                    match found_val {
                        None => CqEntry {
                            tag: entry.tag,
                            status: RingStatus::NotFound as i32,
                            ..CqEntry::default()
                        },
                        Some(val) if val.len() > out_range.len() => CqEntry {
                            tag: entry.tag,
                            status: RingStatus::BufferTooSmall as i32,
                            result_u64: val.len() as u64,
                            ..CqEntry::default()
                        },
                        Some(val) => {
                            guest_memory[out_range.start..out_range.start + val.len()].copy_from_slice(val);
                            CqEntry {
                                tag: entry.tag,
                                status: RingStatus::Ok as i32,
                                result_u64: val.len() as u64,
                                ..CqEntry::default()
                            }
                        }
                    }
                }
                _ => CqEntry {
                    tag: entry.tag,
                    status: RingStatus::Unsupported as i32,
                    ..CqEntry::default()
                },
            };
            cq_out[posted] = cq;
            posted += 1;
        }
        Ok(posted as u32)
    }
}
