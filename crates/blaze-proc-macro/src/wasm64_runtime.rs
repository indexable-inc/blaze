//! Wasmtime `memory64` (`wasm64`) execution harness for Blaze's sans-IO `blaze_host_v1` ABI.
//!
//! Runs `.wasm` modules emitted by `blaze_export::wasm64::export_wasm64` and
//! `blaze_export::lir_wasm64::compile_lir_wasm64` inside a sandboxed Wasmtime store with:
//!
//! - 64-bit linear memory (`wasm_memory64(true)`) and bulk memory (`wasm_bulk_memory(true)`),
//! - deterministic NaN canonicalization (`cranelift_nan_canonicalization(true)`),
//! - fuel metering (`consume_fuel(true)`) and bounded memory/table limits (`StoreLimitsBuilder`),
//! - the 3 `blaze_host_v1` imports (`submit_wait`, `host_caps`, `host_log`), bounds-checked
//!   against the guest's 64-bit linear memory.

use core::fmt::{self, Write as _};

use blaze_own::{AString, AVec, Alloc};
use wasmtime::{
    Caller, Config, Engine, Extern, Instance, Linker, Memory, Module, OptLevel, Store, StoreLimits,
    StoreLimitsBuilder, Trap, Val, WasmBacktraceDetails, WasmParams, WasmResults,
};

use crate::host::{MAX_WASM_STACK, fuel_cost};

/// Formats `args` into an owned `AString<'a>` in `alloc`.
fn own_fmt<'a>(alloc: Alloc<'a>, args: fmt::Arguments<'_>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    let _ = out.write_fmt(args);
    out
}

/// Copies `text` into an owned `AString<'a>` in `alloc`.
fn own_str<'a>(alloc: Alloc<'a>, text: &str) -> AString<'a> {
    let mut out = AString::with_capacity_in(text.len(), alloc);
    out.push_str(text);
    out
}

/// Zero-allocation typed error raised by `blaze_host_v1` callbacks into Wasmtime.
#[derive(Debug)]
enum HostAbiError {
    MissingMemory,
    PtrOutOfRange(i64),
    NegativeLen(i32),
    SliceOverflow { start: usize, len: usize },
    SliceOutOfBounds { start: usize, end: usize, memory_len: usize },
    NegativeSubmitCount { submit: i32, min_complete: i32 },
    InvalidRingPtr(i64),
    RingPtrOutOfBounds { ptr: usize, memory_len: usize },
    InvalidLogUtf8(core::str::Utf8Error),
    LogCountLimit(usize),
    LogBytesLimit(usize),
}

impl fmt::Display for HostAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HostAbiError::MissingMemory => {
                f.write_str("wasm64 guest called host import without exporting `memory`")
            }
            HostAbiError::PtrOutOfRange(ptr) => {
                write!(f, "wasm64 guest pointer {ptr} out of host usize range")
            }
            HostAbiError::NegativeLen(len) => write!(f, "wasm64 guest slice length {len} is negative"),
            HostAbiError::SliceOverflow { start, len } => {
                write!(f, "wasm64 guest slice {start}+{len} overflowed")
            }
            HostAbiError::SliceOutOfBounds { start, end, memory_len } => {
                write!(f, "wasm64 guest slice [{start}..{end}) exceeds linear memory size {memory_len}")
            }
            HostAbiError::NegativeSubmitCount { submit, min_complete } => write!(
                f,
                "blaze_host_v1::submit_wait negative count (submit={submit}, min_complete={min_complete})"
            ),
            HostAbiError::InvalidRingPtr(ptr) => {
                write!(f, "blaze_host_v1::submit_wait invalid ring_ptr {ptr}")
            }
            HostAbiError::RingPtrOutOfBounds { ptr, memory_len } => write!(
                f,
                "blaze_host_v1::submit_wait ring_ptr {ptr} out of bounds for {memory_len}-byte memory"
            ),
            HostAbiError::InvalidLogUtf8(err) => {
                write!(f, "blaze_host_v1::host_log invalid UTF-8: {err}")
            }
            HostAbiError::LogCountLimit(max) => {
                write!(f, "blaze_host_v1::host_log exceeded max_logs ({max})")
            }
            HostAbiError::LogBytesLimit(max) => {
                write!(f, "blaze_host_v1::host_log exceeded max_log_bytes ({max})")
            }
        }
    }
}

impl core::error::Error for HostAbiError {}

/// Canonical import module name for Blaze's Wasm64 sans-IO ring host ABI.
pub const HOST_MODULE: &str = "blaze_host_v1";

/// Resource bounds enforced on each [`Wasm64Instance`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wasm64Limits {
    /// Maximum Wasmtime fuel units this instance may consume across all calls.
    pub fuel: u64,
    /// Maximum linear memory size in bytes.
    pub memory_bytes: usize,
    /// Maximum number of elements in any function/reference table.
    pub table_elements: usize,
    /// Maximum number of `host_log` entries captured per instance.
    pub max_logs: usize,
    /// Maximum total UTF-8 bytes captured across all `host_log` entries.
    pub max_log_bytes: usize,
}

impl Wasm64Limits {
    /// Standard sandbox bounds for executing a Blaze-compiled `wasm64` module.
    pub const STANDARD: Wasm64Limits = Wasm64Limits {
        fuel: 1 << 28,
        memory_bytes: 64 << 20,
        table_elements: 1 << 16,
        max_logs: 4096,
        max_log_bytes: 1 << 20,
    };
}

impl Default for Wasm64Limits {
    fn default() -> Self {
        Self::STANDARD
    }
}

/// Per-instance host state owned by a Wasmtime [`Store`].
#[derive(Debug)]
pub struct Wasm64HostState {
    /// Captured `(level, message)` entries emitted via `blaze_host_v1::host_log`.
    pub logs: AVec<'static, (i32, AString<'static>)>,
    /// Number of `blaze_host_v1::submit_wait` calls made by the guest.
    pub submit_wait_calls: u64,
    /// Number of `blaze_host_v1::host_caps` calls made by the guest.
    pub host_caps_calls: u64,
    /// Capability bitmask advertised to the guest by `blaze_host_v1::host_caps`.
    pub capabilities: u64,
    alloc: Alloc<'static>,
    limits: StoreLimits,
    max_logs: usize,
    max_log_bytes: usize,
    log_bytes: usize,
}

impl Wasm64HostState {
    /// Constructs fresh host state enforcing `limits` using `alloc`.
    pub fn new(limits: Wasm64Limits, alloc: Alloc<'static>) -> Wasm64HostState {
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.memory_bytes)
            .table_elements(limits.table_elements)
            .instances(1)
            .tables(2)
            .memories(1)
            .build();
        Wasm64HostState {
            logs: AVec::new_in(alloc),
            submit_wait_calls: 0,
            host_caps_calls: 0,
            capabilities: 0,
            alloc,
            limits: store_limits,
            max_logs: limits.max_logs,
            max_log_bytes: limits.max_log_bytes,
            log_bytes: 0,
        }
    }

    /// Captured `(level, message)` log entries.
    pub fn logs(&self) -> impl ExactSizeIterator<Item = (i32, &str)> {
        self.logs.iter().map(|(level, text)| (*level, text.as_str()))
    }
}

/// Typed failure returned by [`Wasm64Engine`] and [`Wasm64Instance`].
#[derive(Debug)]
pub enum Wasm64RunError {
    /// Wasmtime refused the `memory64` engine configuration.
    Engine(AString<'static>),
    /// Linking `blaze_host_v1` imports failed.
    Linker(AString<'static>),
    /// Compiling/validating the `.wasm` bytes failed.
    Compile(AString<'static>),
    /// Instantiating the module failed (missing import, memory limit, etc.).
    Instantiate(AString<'static>),
    /// An expected export (function, memory, or global) is missing or has another type.
    MissingExport(AString<'static>),
    /// The guest ran out of its configured fuel budget.
    OutOfFuel { fuel_limit: u64 },
    /// The guest exceeded its configured linear memory limit.
    MemoryLimit { limit: usize, why: AString<'static> },
    /// The guest trapped during execution.
    Trap { code: Option<Trap>, why: AString<'static> },
    /// A host memory read or guest pointer slice was out of linear memory bounds.
    OutOfBounds { offset: u64, len: usize, memory_len: usize },
    /// A guest call failed for another reason (e.g. a host callback refused a bad argument).
    Call { export: AString<'static>, why: AString<'static> },
}

impl fmt::Display for Wasm64RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Wasm64RunError::Engine(why) => write!(f, "wasm64 engine configuration failed: {why}"),
            Wasm64RunError::Linker(why) => write!(f, "wasm64 host import linking failed: {why}"),
            Wasm64RunError::Compile(why) => write!(f, "wasm64 module compilation failed: {why}"),
            Wasm64RunError::Instantiate(why) => write!(f, "wasm64 module instantiation failed: {why}"),
            Wasm64RunError::MissingExport(what) => write!(f, "wasm64 module missing export: {what}"),
            Wasm64RunError::OutOfFuel { fuel_limit } => {
                write!(f, "wasm64 instance exhausted its fuel budget ({fuel_limit})")
            }
            Wasm64RunError::MemoryLimit { limit, why } => {
                write!(f, "wasm64 instance exceeded its {limit}-byte memory limit: {why}")
            }
            Wasm64RunError::Trap { code, why } => match code {
                Some(trap) => write!(f, "wasm64 guest trapped ({trap:?}): {why}"),
                None => write!(f, "wasm64 guest trapped: {why}"),
            },
            Wasm64RunError::OutOfBounds { offset, len, memory_len } => write!(
                f,
                "wasm64 memory read [{offset}..+{len}) is out of bounds for {memory_len}-byte memory"
            ),
            Wasm64RunError::Call { export, why } => write!(f, "wasm64 call to `{export}` failed: {why}"),
        }
    }
}

impl core::error::Error for Wasm64RunError {}

/// Compiled Wasmtime `memory64` engine and pre-wired `blaze_host_v1` linker.
pub struct Wasm64Engine {
    engine: Engine,
    linker: Linker<Wasm64HostState>,
    alloc: Alloc<'static>,
}

impl Wasm64Engine {
    /// Builds a deterministic Wasmtime `memory64` engine with `blaze_host_v1` linked.
    pub fn new(alloc: Alloc<'static>) -> Result<Wasm64Engine, Wasm64RunError> {
        let mut config = Config::new();
        config
            .wasm_memory64(true)
            .wasm_bulk_memory(true)
            .consume_fuel(true)
            .operator_cost(fuel_cost())
            .cranelift_nan_canonicalization(true)
            .cranelift_opt_level(OptLevel::Speed)
            .max_wasm_stack(MAX_WASM_STACK)
            .async_stack_size(MAX_WASM_STACK)
            .wasm_backtrace_details(WasmBacktraceDetails::Disable);
        let engine = Engine::new(&config)
            .map_err(|err| Wasm64RunError::Engine(own_fmt(alloc, format_args!("{err:#}"))))?;
        let mut linker = Linker::new(&engine);
        link_blaze_host_v1(&mut linker)
            .map_err(|err| Wasm64RunError::Linker(own_fmt(alloc, format_args!("{err:#}"))))?;
        Ok(Wasm64Engine { engine, linker, alloc })
    }

    /// Underlying Wasmtime [`Engine`].
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compiles and instantiates a `memory64` `.wasm` binary under `limits`.
    pub fn instantiate(&self, wasm: &[u8], limits: Wasm64Limits) -> Result<Wasm64Instance, Wasm64RunError> {
        let alloc = self.alloc;
        let module = Module::new(&self.engine, wasm)
            .map_err(|err| Wasm64RunError::Compile(own_fmt(alloc, format_args!("{err:#}"))))?;
        let mut store = Store::new(&self.engine, Wasm64HostState::new(limits, alloc));
        store.limiter(|state| &mut state.limits);
        store.set_fuel(limits.fuel).map_err(|err| {
            Wasm64RunError::Instantiate(own_fmt(alloc, format_args!("set_fuel failed: {err:#}")))
        })?;
        let instance = self.linker.instantiate(&mut store, &module).map_err(|err| {
            let msg = own_fmt(alloc, format_args!("{err:#}"));
            if msg.as_str().contains("memory minimum size")
                || (msg.as_str().contains("memory") && msg.as_str().contains("limit"))
            {
                Wasm64RunError::MemoryLimit { limit: limits.memory_bytes, why: msg }
            } else {
                Wasm64RunError::Instantiate(msg)
            }
        })?;
        let Some(memory) = instance.get_memory(&mut store, "memory") else {
            return Err(Wasm64RunError::MissingExport(own_str(alloc, "memory")));
        };
        if !memory.ty(&store).is_64() {
            return Err(Wasm64RunError::Instantiate(own_str(
                alloc,
                "exported `memory` is 32-bit; Wasm64Instance requires memory64",
            )));
        }
        Ok(Wasm64Instance {
            store,
            instance,
            memory,
            alloc,
            fuel_limit: limits.fuel,
            memory_limit: limits.memory_bytes,
        })
    }
}

/// Resolves exported `memory` on `caller`, failing loudly if the module exports none.
fn caller_memory(caller: &mut Caller<'_, Wasm64HostState>) -> wasmtime::Result<Memory> {
    caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::new(HostAbiError::MissingMemory))
}

/// Validates and extracts an immutable byte slice `[ptr..ptr+len)` from 64-bit guest linear memory.
fn guest_slice(mem: &[u8], ptr: i64, len: i32) -> wasmtime::Result<&[u8]> {
    let start = usize::try_from(ptr).map_err(|_| wasmtime::Error::new(HostAbiError::PtrOutOfRange(ptr)))?;
    let len = usize::try_from(len).map_err(|_| wasmtime::Error::new(HostAbiError::NegativeLen(len)))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| wasmtime::Error::new(HostAbiError::SliceOverflow { start, len }))?;
    mem.get(start..end).ok_or_else(|| {
        wasmtime::Error::new(HostAbiError::SliceOutOfBounds { start, end, memory_len: mem.len() })
    })
}

/// Validates and extracts a mutable byte slice `[ptr..ptr+len)` from 64-bit guest linear memory.
fn guest_slice_mut(mem: &mut [u8], ptr: i64, len: i32) -> wasmtime::Result<&mut [u8]> {
    let memory_len = mem.len();
    let start = usize::try_from(ptr).map_err(|_| wasmtime::Error::new(HostAbiError::PtrOutOfRange(ptr)))?;
    let len = usize::try_from(len).map_err(|_| wasmtime::Error::new(HostAbiError::NegativeLen(len)))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| wasmtime::Error::new(HostAbiError::SliceOverflow { start, len }))?;
    mem.get_mut(start..end)
        .ok_or_else(|| wasmtime::Error::new(HostAbiError::SliceOutOfBounds { start, end, memory_len }))
}

/// Links the 3 `blaze_host_v1` sans-IO ring host imports onto `linker`.
fn link_blaze_host_v1(linker: &mut Linker<Wasm64HostState>) -> wasmtime::Result<()> {
    // Type 0: submit_wait(ring_ptr: i64, submit_count: i32, min_complete: i32, timeout_ns: i64) -> i32
    linker.func_wrap(
        HOST_MODULE,
        "submit_wait",
        |mut caller: Caller<'_, Wasm64HostState>,
         ring_ptr: i64,
         submit_count: i32,
         min_complete: i32,
         _timeout_ns: i64|
         -> wasmtime::Result<i32> {
            if submit_count < 0 || min_complete < 0 {
                return Err(wasmtime::Error::new(HostAbiError::NegativeSubmitCount {
                    submit: submit_count,
                    min_complete,
                }));
            }
            let memory = caller_memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            if ring_ptr != 0 || submit_count > 0 {
                let ptr = usize::try_from(ring_ptr)
                    .map_err(|_| wasmtime::Error::new(HostAbiError::InvalidRingPtr(ring_ptr)))?;
                if ptr >= mem.len() {
                    return Err(wasmtime::Error::new(HostAbiError::RingPtrOutOfBounds {
                        ptr,
                        memory_len: mem.len(),
                    }));
                }
            }
            state.submit_wait_calls += 1;
            Ok(0)
        },
    )?;

    // Type 1: host_caps(out_ptr: i64, out_len: i32) -> i32
    linker.func_wrap(
        HOST_MODULE,
        "host_caps",
        |mut caller: Caller<'_, Wasm64HostState>, out_ptr: i64, out_len: i32| -> wasmtime::Result<i32> {
            let memory = caller_memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            state.host_caps_calls += 1;
            let dst = guest_slice_mut(mem, out_ptr, out_len)?;
            let caps = state.capabilities.to_le_bytes();
            let copy_len = dst.len().min(caps.len());
            dst[..copy_len].copy_from_slice(&caps[..copy_len]);
            Ok(i32::try_from(copy_len).unwrap_or(i32::MAX))
        },
    )?;

    // Type 2: host_log(level: i32, ptr: i64, len: i32) -> ()
    linker.func_wrap(
        HOST_MODULE,
        "host_log",
        |mut caller: Caller<'_, Wasm64HostState>, level: i32, ptr: i64, len: i32| -> wasmtime::Result<()> {
            let memory = caller_memory(&mut caller)?;
            let (mem, state) = memory.data_and_store_mut(&mut caller);
            let bytes = guest_slice(mem, ptr, len)?;
            let text = core::str::from_utf8(bytes)
                .map_err(|err| wasmtime::Error::new(HostAbiError::InvalidLogUtf8(err)))?;
            if state.logs.len() >= state.max_logs {
                return Err(wasmtime::Error::new(HostAbiError::LogCountLimit(state.max_logs)));
            }
            let next_bytes = state.log_bytes.saturating_add(text.len());
            if next_bytes > state.max_log_bytes {
                return Err(wasmtime::Error::new(HostAbiError::LogBytesLimit(state.max_log_bytes)));
            }
            state.log_bytes = next_bytes;
            let owned = own_str(state.alloc, text);
            state.logs.push((level, owned));
            Ok(())
        },
    )?;

    Ok(())
}

/// Live instantiated `memory64` module with typed invocation and memory inspection helpers.
pub struct Wasm64Instance {
    store: Store<Wasm64HostState>,
    instance: Instance,
    memory: Memory,
    alloc: Alloc<'static>,
    fuel_limit: u64,
    memory_limit: usize,
}

impl Wasm64Instance {
    fn classify_error(&self, export: &str, err: wasmtime::Error) -> Wasm64RunError {
        let trap = err.downcast_ref::<Trap>().copied();
        if trap == Some(Trap::OutOfFuel) {
            return Wasm64RunError::OutOfFuel { fuel_limit: self.fuel_limit };
        }
        let msg = own_fmt(self.alloc, format_args!("{err:#}"));
        if msg.as_str().contains("memory") && msg.as_str().contains("limit") {
            return Wasm64RunError::MemoryLimit { limit: self.memory_limit, why: msg };
        }
        if trap.is_some() {
            return Wasm64RunError::Trap { code: trap, why: msg };
        }
        Wasm64RunError::Call { export: own_str(self.alloc, export), why: msg }
    }

    fn call_typed<P, R>(&mut self, name: &str, params: P) -> Result<R, Wasm64RunError>
    where
        P: WasmParams,
        R: WasmResults,
    {
        let alloc = self.alloc;
        let func = self
            .instance
            .get_typed_func::<P, R>(&mut self.store, name)
            .map_err(|err| Wasm64RunError::MissingExport(own_fmt(alloc, format_args!("{name}: {err:#}"))))?;
        func.call(&mut self.store, params)
            .map_err(|err| self.classify_error(name, err))
    }

    /// Invokes the required `_start: () -> ()` export.
    pub fn call_start(&mut self) -> Result<(), Wasm64RunError> {
        self.call_typed::<(), ()>("_start", ())
    }

    /// Invokes an exported `() -> i64` function.
    pub fn call_i64_0(&mut self, name: &str) -> Result<i64, Wasm64RunError> {
        self.call_typed::<(), i64>(name, ())
    }

    /// Invokes an exported `(i64) -> i64` function.
    pub fn call_i64_1(&mut self, name: &str, a: i64) -> Result<i64, Wasm64RunError> {
        self.call_typed::<i64, i64>(name, a)
    }

    /// Invokes an exported `(i64, i64) -> i64` function.
    pub fn call_i64_2(&mut self, name: &str, a: i64, b: i64) -> Result<i64, Wasm64RunError> {
        self.call_typed::<(i64, i64), i64>(name, (a, b))
    }

    /// Invokes an exported `() -> i32` function.
    pub fn call_i32_0(&mut self, name: &str) -> Result<i32, Wasm64RunError> {
        self.call_typed::<(), i32>(name, ())
    }

    /// Invokes an exported `(i32) -> i32` function.
    pub fn call_i32_1(&mut self, name: &str, a: i32) -> Result<i32, Wasm64RunError> {
        self.call_typed::<i32, i32>(name, a)
    }

    /// Invokes an exported `(i32, i32) -> i32` function.
    pub fn call_i32_2(&mut self, name: &str, a: i32, b: i32) -> Result<i32, Wasm64RunError> {
        self.call_typed::<(i32, i32), i32>(name, (a, b))
    }

    /// Invokes an exported `(f64) -> f64` function.
    pub fn call_f64_1(&mut self, name: &str, a: f64) -> Result<f64, Wasm64RunError> {
        self.call_typed::<f64, f64>(name, a)
    }

    /// Invokes an exported `(f64, f64) -> f64` function.
    pub fn call_f64_2(&mut self, name: &str, a: f64, b: f64) -> Result<f64, Wasm64RunError> {
        self.call_typed::<(f64, f64), f64>(name, (a, b))
    }

    /// Reads `[offset..offset+len)` from the guest's 64-bit linear memory.
    pub fn read_memory(&self, offset: u64, len: usize) -> Result<&[u8], Wasm64RunError> {
        let mem = self.memory.data(&self.store);
        let start = usize::try_from(offset).map_err(|_| Wasm64RunError::OutOfBounds {
            offset,
            len,
            memory_len: mem.len(),
        })?;
        let end = start.checked_add(len).ok_or(Wasm64RunError::OutOfBounds {
            offset,
            len,
            memory_len: mem.len(),
        })?;
        mem.get(start..end).ok_or(Wasm64RunError::OutOfBounds {
            offset,
            len,
            memory_len: mem.len(),
        })
    }

    /// Reads an exported `i64` global (e.g. `$__stack_pointer` if exported).
    pub fn read_i64_global(&mut self, name: &str) -> Result<i64, Wasm64RunError> {
        let alloc = self.alloc;
        let Some(global) = self.instance.get_global(&mut self.store, name) else {
            return Err(Wasm64RunError::MissingExport(own_str(alloc, name)));
        };
        match global.get(&mut self.store) {
            Val::I64(value) => Ok(value),
            other => Err(Wasm64RunError::MissingExport(own_fmt(
                alloc,
                format_args!("global `{name}` has type {:?}, expected i64", other.ty(&self.store)),
            ))),
        }
    }

    /// Reads an exported `i32` global (e.g. `blaze_ring_abi_version`).
    pub fn read_i32_global(&mut self, name: &str) -> Result<i32, Wasm64RunError> {
        let alloc = self.alloc;
        let Some(global) = self.instance.get_global(&mut self.store, name) else {
            return Err(Wasm64RunError::MissingExport(own_str(alloc, name)));
        };
        match global.get(&mut self.store) {
            Val::I32(value) => Ok(value),
            other => Err(Wasm64RunError::MissingExport(own_fmt(
                alloc,
                format_args!("global `{name}` has type {:?}, expected i32", other.ty(&self.store)),
            ))),
        }
    }

    /// Returns true because [`Wasm64Engine::instantiate`] enforces 64-bit linear memory.
    pub fn memory_is_64(&self) -> bool {
        self.memory.ty(&self.store).is_64()
    }

    /// Current linear memory size in bytes.
    pub fn memory_byte_len(&self) -> usize {
        self.memory.data(&self.store).len()
    }

    /// Remaining fuel units in this store.
    pub fn fuel_remaining(&self) -> u64 {
        self.store.get_fuel().unwrap_or(0)
    }

    /// Fuel units consumed so far by this instance.
    pub fn fuel_consumed(&self) -> u64 {
        self.fuel_limit.saturating_sub(self.fuel_remaining())
    }

    /// Accesses the store's [`Wasm64HostState`].
    pub fn host_state(&self) -> &Wasm64HostState {
        self.store.data()
    }

    /// Captured `blaze_host_v1::host_log` messages.
    pub fn logs(&self) -> impl ExactSizeIterator<Item = (i32, &str)> {
        self.store.data().logs()
    }
}

#[cfg(test)]
mod tests {
    use blaze_export::{
        lir_wasm64::{
            LirWasm64Func, LirWasm64Import, LirWasm64Module, LirWasm64Static, compile_lir_wasm64,
        },
        wasm64::{HOST_MODULE as EXPORT_HOST_MODULE, RING_ABI_VERSION, Wasm64Opts, check_wasm64},
    };
    use blaze_lir::{
        BlockKind, Builder, Flags, Full, Func, IntCond, Op, Ty, View,
        body::Target,
        builder::{MemAttrs, Param, Variadic},
        eval::{Bodies, BodyId, Datum, DatumId, Outcome, Trap as EvalTrap, Val, run_calls_in, run_in},
        func::{Conv, RefId, RefKind, SlotKind},
    };
    use blaze_own::{AVec, Alloc};

    use super::{HOST_MODULE, Wasm64Engine, Wasm64Instance, Wasm64Limits, Wasm64RunError};

    const EVAL_FUEL: u64 = 1_000_000;

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn begin_fn(params: &[Ty], rets: &[Ty]) -> Builder<Alloc<'static>, Full> {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let mut sig_params = blaze_own::Vec::with_capacity_in(params.len(), alloc);
        for &ty in params {
            sig_params.push(Param::of(ty));
        }
        let mut sig_rets = blaze_own::Vec::with_capacity_in(rets.len(), alloc);
        for &ty in rets {
            sig_rets.push(Param::of(ty));
        }
        let sig = b.sig(Conv::C, Variadic::No, &sig_params, &sig_rets);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        b
    }

    /// Minimal `_start() -> ()` stub required by `export_wasm64` / `check_wasm64`.
    fn build_empty_start() -> Func<Alloc<'static>> {
        let mut b = begin_fn(&[], &[]);
        b.ret(&[]);
        b.finish_checked().expect("_start builds")
    }

    /// Compiles a `LirWasm64Module`, validates with `check_wasm64`, and instantiates in Wasmtime.
    fn compile_and_instantiate(
        module: &LirWasm64Module<'_>,
        limits: Wasm64Limits,
    ) -> (Wasm64Instance, AVec<'static, u8>) {
        let alloc = test_alloc();
        let mut wasm = AVec::new_in(alloc);
        let exported = compile_lir_wasm64(module, &mut wasm, alloc)
            .unwrap_or_else(|err| panic!("compile_lir_wasm64 failed for {}: {err}", module.name));
        let checked = check_wasm64(&wasm)
            .unwrap_or_else(|err| panic!("check_wasm64 failed for {}: {err}", module.name));
        assert!(checked.memory64, "module must use memory64");
        assert!(checked.has_memory_export, "module must export memory");
        assert!(checked.has_start_export, "module must export _start");
        assert_eq!(exported.len as usize, wasm.len());

        let engine = Wasm64Engine::new(alloc).expect("Wasm64Engine initializes");
        let mut inst = engine
            .instantiate(&wasm, limits)
            .unwrap_or_else(|err| panic!("Wasmtime instantiation failed for {}: {err}", module.name));
        assert!(inst.memory_is_64());
        assert_eq!(
            inst.read_i32_global("blaze_ring_abi_version").expect("abi version global"),
            RING_ABI_VERSION
        );
        (inst, wasm)
    }

    fn eval_single_i64(func: &Func<Alloc<'static>>, args: &[Val]) -> i64 {
        let outcome = run_in(func.view(), args, EVAL_FUEL, test_alloc()).expect("blaze_lir::eval runs");
        match outcome {
            Outcome::Returned(vals) => match vals.as_slice() {
                [Val::Int(bits)] => (*bits as u64) as i64,
                other => panic!("expected single Int return, got {other:?}"),
            },
            other => panic!("expected Returned outcome, got {other:?}"),
        }
    }

    fn eval_single_i32(func: &Func<Alloc<'static>>, args: &[Val]) -> i32 {
        let outcome = run_in(func.view(), args, EVAL_FUEL, test_alloc()).expect("blaze_lir::eval runs");
        match outcome {
            Outcome::Returned(vals) => match vals.as_slice() {
                [Val::Int(bits)] => (*bits as u32) as i32,
                other => panic!("expected single Int return, got {other:?}"),
            },
            other => panic!("expected Returned outcome, got {other:?}"),
        }
    }

    fn eval_single_f64_bits(func: &Func<Alloc<'static>>, args: &[Val]) -> u64 {
        let outcome = run_in(func.view(), args, EVAL_FUEL, test_alloc()).expect("blaze_lir::eval runs");
        match outcome {
            Outcome::Returned(vals) => match vals.as_slice() {
                [Val::Int(bits)] => *bits as u64,
                other => panic!("expected single f64 Int-bits return, got {other:?}"),
            },
            other => panic!("expected Returned outcome, got {other:?}"),
        }
    }

    fn body_id_for_place(place: u32) -> BodyId {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&place.to_le_bytes());
        BodyId(bytes)
    }

    fn datum_id_for_place(place: u32) -> DatumId {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&place.to_le_bytes());
        DatumId(bytes)
    }

    /// Small `Bodies` table for multi-function / static-data `blaze_lir::eval::run_calls_in` tests.
    struct TableBodies<'a> {
        funcs: &'a [(u32, View<'a>)],
        statics: &'a [(u32, &'a [u8], u8)],
    }

    impl<'a> TableBodies<'a> {
        fn find_func(&self, place: u32) -> Option<View<'a>> {
            self.funcs.iter().find_map(|&(p, view)| (p == place).then_some(view))
        }

        fn find_func_by_id(&self, id: BodyId) -> Option<View<'a>> {
            let place = u32::from_le_bytes([id.0[0], id.0[1], id.0[2], id.0[3]]);
            self.find_func(place)
        }
    }

    impl Bodies for TableBodies<'_> {
        fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
            let caller = self.find_func_by_id(from)?;
            let rec = caller.reference(reference);
            if rec.kind != RefKind::Func as u32 {
                return None;
            }
            let view = self.find_func(rec.place)?;
            Some((body_id_for_place(rec.place), view))
        }

        fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
            let caller = self.find_func_by_id(from)?;
            let rec = caller.reference(reference);
            match RefKind::from_word(rec.kind)? {
                RefKind::Func => self
                    .find_func(rec.place)
                    .map(|_| Target::Func(body_id_for_place(rec.place))),
                RefKind::Static => self
                    .statics
                    .iter()
                    .find(|&&(p, _, _)| p == rec.place)
                    .map(|_| Target::Datum(datum_id_for_place(rec.place))),
                _ => None,
            }
        }

        fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
            let place = u32::from_le_bytes([id.0[0], id.0[1], id.0[2], id.0[3]]);
            let &(_, bytes, align) = self.statics.iter().find(|&&(p, _, _)| p == place)?;
            Some(Datum { bytes, align, relocs: &[], writable: false })
        }

        fn body(&self, id: BodyId) -> Option<View<'_>> {
            self.find_func_by_id(id)
        }
    }

    // ---- 1. Straight-line arithmetic, bitwise, shifts, comparisons, Select ------------------

    #[test]
    fn oracle_straight_line_arithmetic_bitwise_shifts_cmp_select() {
        assert_eq!(HOST_MODULE, EXPORT_HOST_MODULE);

        // (a: i64, b: i64) -> i64 exercising i64 arithmetic, bitwise, shifts, rotates, counts, min/max, icmp, select
        let arith64 = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I64]);
            let a = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let sum = b.binary(Op::Add, Flags::NONE, a, y);
            let diff = b.binary(Op::Sub, Flags::NONE, a, y);
            let prod = b.binary(Op::Mul, Flags::NONE, sum, diff);
            let neg_y = b.unary(Op::Neg, Flags::NONE, y);
            let xor_v = b.binary(Op::Xor, Flags::NONE, prod, neg_y);
            let and_v = b.binary(Op::And, Flags::NONE, xor_v, a);
            let or_v = b.binary(Op::Or, Flags::NONE, and_v, y);
            let not_v = b.unary(Op::Not, Flags::NONE, or_v);
            let shl_v = b.binary(Op::Shl, Flags::NONE, a, y);
            let lshr_v = b.binary(Op::Lshr, Flags::NONE, not_v, y);
            let ashr_v = b.binary(Op::Ashr, Flags::NONE, diff, y);
            let rotl_v = b.binary(Op::Rotl, Flags::NONE, shl_v, y);
            let rotr_v = b.binary(Op::Rotr, Flags::NONE, lshr_v, a);
            let clz_v = b.unary(Op::Clz, Flags::NONE, a);
            let ctz_v = b.unary(Op::Ctz, Flags::NONE, y);
            let pop_v = b.unary(Op::Popcnt, Flags::NONE, xor_v);
            let umin_v = b.binary(Op::Umin, Flags::NONE, rotl_v, rotr_v);
            let smax_v = b.binary(Op::Smax, Flags::NONE, ashr_v, prod);
            let cond = b.icmp(IntCond::Slt, a, y);
            let picked = b.select(Flags::NONE, cond, umin_v, smax_v);
            let t1 = b.binary(Op::Add, Flags::NONE, picked, clz_v);
            let t2 = b.binary(Op::Xor, Flags::NONE, t1, ctz_v);
            let out = b.binary(Op::Add, Flags::NONE, t2, pop_v);
            b.ret(&[out]);
            b.finish_checked().expect("arith64 builds")
        };

        // (a: i32, b: i32) -> i32 exercising i8/i16/i32 signed & unsigned edge cases, shifts, div/rem, icmp, select
        let subword = {
            let mut b = begin_fn(&[Ty::I32, Ty::I32], &[Ty::I32]);
            let a = b.param(Ty::I32);
            let y = b.param(Ty::I32);

            let a8 = b.convert(Op::Trunc, Flags::NONE, Ty::I8, a);
            let y8 = b.convert(Op::Trunc, Flags::NONE, Ty::I8, y);
            let add8 = b.binary(Op::Add, Flags::NONE, a8, y8);
            let ashr8 = b.binary(Op::Ashr, Flags::NONE, a8, y8);
            let lshr8 = b.binary(Op::Lshr, Flags::NONE, a8, y8);
            let slt8 = b.icmp(IntCond::Slt, a8, y8);
            let ult8 = b.icmp(IntCond::Ult, a8, y8);
            let sel8 = b.select(Flags::NONE, slt8, ashr8, lshr8);
            let comb8 = b.binary(Op::Xor, Flags::NONE, add8, sel8);
            let sext8 = b.convert(Op::Sext, Flags::NONE, Ty::I32, comb8);
            let ult8_ext = b.convert(Op::Zext, Flags::NONE, Ty::I32, ult8);

            let a16 = b.convert(Op::Trunc, Flags::NONE, Ty::I16, a);
            let y16 = b.convert(Op::Trunc, Flags::NONE, Ty::I16, y);
            let one16 = b.iconst(Ty::I16, 1);
            let mask16 = b.iconst(Ty::I16, 0x00FF);
            // Safe nonzero divisor in [1..=255] so neither udiv nor sdiv can trap or hit MIN/-1.
            let y16_m = b.binary(Op::And, Flags::NONE, y16, mask16);
            let denom16 = b.binary(Op::Or, Flags::NONE, y16_m, one16);
            let sdiv16 = b.binary(Op::Sdiv, Flags::NONE, a16, denom16);
            let udiv16 = b.binary(Op::Udiv, Flags::NONE, a16, denom16);
            let urem16 = b.binary(Op::Urem, Flags::NONE, a16, denom16);
            let srem16 = b.binary(Op::Srem, Flags::NONE, a16, denom16);
            let mix16_a = b.binary(Op::Sub, Flags::NONE, sdiv16, udiv16);
            let mix16_b = b.binary(Op::Xor, Flags::NONE, urem16, srem16);
            let mix16 = b.binary(Op::Add, Flags::NONE, mix16_a, mix16_b);
            let zext16 = b.convert(Op::Zext, Flags::NONE, Ty::I32, mix16);

            let c1 = b.binary(Op::Add, Flags::NONE, sext8, zext16);
            let c2 = b.binary(Op::Xor, Flags::NONE, c1, ult8_ext);
            b.ret(&[c2]);
            b.finish_checked().expect("subword builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "arith64", export_name: Some("arith64"), place: 1, view: arith64.view() },
            LirWasm64Func { name: "subword", export_name: Some("subword"), place: 2, view: subword.view() },
        ];
        let module = LirWasm64Module {
            name: "oracle_straight_line",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);
        inst.call_start().expect("_start runs");

        let i64_cases: &[(i64, i64)] = &[
            (0, 0),
            (1, 2),
            (-1, 1),
            (i64::MIN, 1),
            (i64::MAX, -1),
            (i64::MIN, i64::MAX),
            (0x0123_4567_89AB_CDEF, 63),
            (-42, 65), // shift amount >= 64 masks modulo 64
        ];
        for &(a, b) in i64_cases {
            let got = inst.call_i64_2("arith64", a, b).expect("arith64 runs");
            let want = eval_single_i64(&arith64, &[Val::Int(a as u64 as u128), Val::Int(b as u64 as u128)]);
            assert_eq!(got, want, "arith64({a:#x}, {b:#x}) mismatch");
        }

        let i32_cases: &[(i32, i32)] = &[
            (0, 0),
            (127, 1),
            (-128, 1),
            (-1, 0),
            (255, 128),
            (0x7FFF, 3),
            (-32768, 7),
            (0x00FF_8000, -17),
            (i32::MIN, i32::MAX),
        ];
        for &(a, b) in i32_cases {
            let got = inst.call_i32_2("subword", a, b).expect("subword runs");
            let want = eval_single_i32(&subword, &[Val::Int(a as u32 as u128), Val::Int(b as u32 as u128)]);
            assert_eq!(got, want, "subword({a:#x}, {b:#x}) mismatch");
        }
    }

    // ---- 2. Multi-block loops (`iterative_fib` + `sum_to_n`) across 0..=20 ------------------

    #[test]
    fn oracle_multi_block_loops_fib_and_sum_to_n() {
        let fib = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let loop_hdr = b.create_block(BlockKind::Normal);
            let loop_body = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);

            let n = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            b.jump((loop_hdr, &[zero, zero, one]));

            b.begin(loop_hdr);
            let i = b.param(Ty::I64);
            let a = b.param(Ty::I64);
            let cur = b.param(Ty::I64);
            let keep_going = b.icmp(IntCond::Slt, i, n);
            b.br(Flags::NONE, keep_going, (loop_body, &[]), (exit, &[a]));

            b.begin(loop_body);
            let next_i = b.binary(Op::Add, Flags::NONE, i, one);
            let next_cur = b.binary(Op::Add, Flags::NONE, a, cur);
            // Parallel block-arg transfer `(i, a, cur) <- (next_i, cur, next_cur)` tests copy-cycle handling.
            b.jump((loop_hdr, &[next_i, cur, next_cur]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().expect("iterative_fib builds")
        };

        let sum_to_n = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let hdr = b.create_block(BlockKind::Normal);
            let body = b.create_block(BlockKind::Normal);
            let done = b.create_block(BlockKind::Normal);

            let n = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            b.jump((hdr, &[one, zero]));

            b.begin(hdr);
            let k = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let le = b.icmp(IntCond::Sle, k, n);
            b.br(Flags::NONE, le, (body, &[]), (done, &[acc]));

            b.begin(body);
            let next_acc = b.binary(Op::Add, Flags::NONE, acc, k);
            let next_k = b.binary(Op::Add, Flags::NONE, k, one);
            b.jump((hdr, &[next_k, next_acc]));

            b.begin(done);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().expect("sum_to_n builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "iterative_fib", export_name: Some("iterative_fib"), place: 1, view: fib.view() },
            LirWasm64Func { name: "sum_to_n", export_name: Some("sum_to_n"), place: 2, view: sum_to_n.view() },
        ];
        let module = LirWasm64Module {
            name: "oracle_loops",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);
        for n in 0i64..=20 {
            let got_fib = inst.call_i64_1("iterative_fib", n).expect("fib runs");
            let want_fib = eval_single_i64(&fib, &[Val::Int(n as u64 as u128)]);
            assert_eq!(got_fib, want_fib, "iterative_fib({n}) mismatch");

            let got_sum = inst.call_i64_1("sum_to_n", n).expect("sum_to_n runs");
            let want_sum = eval_single_i64(&sum_to_n, &[Val::Int(n as u64 as u128)]);
            assert_eq!(got_sum, want_sum, "sum_to_n({n}) mismatch");
            assert_eq!(got_sum, n * (n + 1) / 2);
        }
        assert_eq!(inst.call_i64_1("iterative_fib", 20).expect("fib(20)"), 6765);
    }

    // ---- 3. Recursive functions (`recursive_fact` + `gcd`) + multi-function direct `Call` ---

    #[test]
    fn oracle_recursive_fact_gcd_and_multi_func_calls() {
        const PLACE_FACT: u32 = 1;
        const PLACE_GCD: u32 = 2;
        const PLACE_HELPER: u32 = 3;
        const PLACE_MAIN: u32 = 4;

        let fact = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let self_ref = b.reference(RefKind::Func, PLACE_FACT);
            let base = b.create_block(BlockKind::Normal);
            let rec = b.create_block(BlockKind::Normal);

            let n = b.param(Ty::I64);
            let one = b.iconst(Ty::I64, 1);
            let is_base = b.icmp(IntCond::Sle, n, one);
            b.br(Flags::NONE, is_base, (base, &[]), (rec, &[]));

            b.begin(base);
            b.ret(&[one]);

            b.begin(rec);
            let nm1 = b.binary(Op::Sub, Flags::NONE, n, one);
            let sub = b.call(Flags::NONE, self_ref, sig, &[nm1]);
            let prod = b.binary(Op::Mul, Flags::NONE, n, sub);
            b.ret(&[prod]);
            b.finish_checked().expect("recursive_fact builds")
        };

        let gcd = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I64]);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            let self_ref = b.reference(RefKind::Func, PLACE_GCD);
            let done = b.create_block(BlockKind::Normal);
            let step = b.create_block(BlockKind::Normal);

            let a = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let is_zero = b.icmp(IntCond::Eq, y, zero);
            b.br(Flags::NONE, is_zero, (done, &[]), (step, &[]));

            b.begin(done);
            b.ret(&[a]);

            b.begin(step);
            let rem = b.binary(Op::Urem, Flags::NONE, a, y);
            let res = b.call(Flags::NONE, self_ref, sig, &[y, rem]);
            b.ret(&[res]);
            b.finish_checked().expect("gcd builds")
        };

        // helper(x, y) = recursive_fact(x) + gcd(x * 12, y * 12)
        let helper = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I64]);
            let sig1 = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let sig2 = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            let fact_ref = b.reference(RefKind::Func, PLACE_FACT);
            let gcd_ref = b.reference(RefKind::Func, PLACE_GCD);

            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let twelve = b.iconst(Ty::I64, 12);
            let f = b.call(Flags::NONE, fact_ref, sig1, &[x]);
            let x12 = b.binary(Op::Mul, Flags::NONE, x, twelve);
            let y12 = b.binary(Op::Mul, Flags::NONE, y, twelve);
            let g = b.call(Flags::NONE, gcd_ref, sig2, &[x12, y12]);
            let sum = b.binary(Op::Add, Flags::NONE, f, g);
            b.ret(&[sum]);
            b.finish_checked().expect("helper builds")
        };

        // main_calc(x, y) = helper(x, y) * 3 + 5
        let main_calc = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I64]);
            let sig2 = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            let helper_ref = b.reference(RefKind::Func, PLACE_HELPER);

            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let h = b.call(Flags::NONE, helper_ref, sig2, &[x, y]);
            let three = b.iconst(Ty::I64, 3);
            let five = b.iconst(Ty::I64, 5);
            let scaled = b.binary(Op::Mul, Flags::NONE, h, three);
            let out = b.binary(Op::Add, Flags::NONE, scaled, five);
            b.ret(&[out]);
            b.finish_checked().expect("main_calc builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "fact", export_name: Some("fact"), place: PLACE_FACT, view: fact.view() },
            LirWasm64Func { name: "gcd", export_name: Some("gcd"), place: PLACE_GCD, view: gcd.view() },
            LirWasm64Func { name: "helper", export_name: Some("helper"), place: PLACE_HELPER, view: helper.view() },
            LirWasm64Func { name: "main_calc", export_name: Some("main_calc"), place: PLACE_MAIN, view: main_calc.view() },
        ];
        let module = LirWasm64Module {
            name: "oracle_recursion_and_calls",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let table_funcs = [
            (PLACE_FACT, fact.view()),
            (PLACE_GCD, gcd.view()),
            (PLACE_HELPER, helper.view()),
            (PLACE_MAIN, main_calc.view()),
        ];
        let bodies = TableBodies { funcs: &table_funcs, statics: &[] };
        let eval_call = |place: u32, view: View<'_>, args: &[Val]| -> i64 {
            let outcome = run_calls_in(view, body_id_for_place(place), args, EVAL_FUEL, &bodies, test_alloc())
                .expect("eval run_calls_in succeeds");
            match outcome {
                Outcome::Returned(vals) => match vals.as_slice() {
                    [Val::Int(bits)] => (*bits as u64) as i64,
                    other => panic!("unexpected eval return {other:?}"),
                },
                other => panic!("unexpected eval outcome {other:?}"),
            }
        };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);
        for n in 0i64..=12 {
            let got = inst.call_i64_1("fact", n).expect("fact runs");
            let want = eval_call(PLACE_FACT, fact.view(), &[Val::Int(n as u64 as u128)]);
            assert_eq!(got, want, "fact({n}) mismatch");
        }
        for &(a, b) in &[(48i64, 18), (1071, 462), (17, 13), (0, 9), (9, 0)] {
            let got = inst.call_i64_2("gcd", a, b).expect("gcd runs");
            let want = eval_call(
                PLACE_GCD,
                gcd.view(),
                &[Val::Int(a as u64 as u128), Val::Int(b as u64 as u128)],
            );
            assert_eq!(got, want, "gcd({a}, {b}) mismatch");
        }
        for &(x, y) in &[(5i64, 15), (7, 21), (10, 4)] {
            let got = inst.call_i64_2("main_calc", x, y).expect("main_calc runs");
            let want = eval_call(
                PLACE_MAIN,
                main_calc.view(),
                &[Val::Int(x as u64 as u128), Val::Int(y as u64 as u128)],
            );
            assert_eq!(got, want, "main_calc({x}, {y}) mismatch");
        }
    }

    // ---- 4. Shadow-stack frame (`SlotAddr`, `Load`, `Store`, `Memcpy`, `Memset`, `Memcmp`) --

    #[test]
    fn oracle_shadow_stack_frame_load_store_memcpy_memset_memcmp() {
        // Allocates 128 bytes of shadow-stack slots (`buf_a`: 64, `buf_b`: 64), runs Memset,
        // Store, Memcpy, Memcmp (equal, less-than, greater-than) and Load.
        let stack_ops = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let seed = b.param(Ty::I64);

            let buf_a = b.slot(64, 3, SlotKind::Named);
            let buf_b = b.slot(64, 3, SlotKind::Named);
            let pa = b.slot_addr(buf_a);
            let pb = b.slot_addr(buf_b);

            let len64 = b.iconst(Ty::I64, 64);
            let fill_byte = b.convert(Op::Trunc, Flags::NONE, Ty::I8, seed);
            b.bulk(Op::Memset, Flags::NONE, pa, fill_byte, len64, MemAttrs::aligned(3));

            let xor_const = b.iconst(Ty::I64, 0x1357_9BDF_2468_ACE0);
            let val2 = b.binary(Op::Xor, Flags::NONE, seed, xor_const);
            b.store(Flags::NONE, seed, pa, 8, MemAttrs::aligned(3));
            b.store(Flags::NONE, val2, pa, 16, MemAttrs::aligned(3));

            b.bulk(
                Op::Memcpy,
                Flags::NONE,
                pb,
                pa,
                len64,
                MemAttrs { src_align: 3, ..MemAttrs::aligned(3) },
            );

            // Equal buffers => memcmp == 0
            let cmp_eq = b.memcmp(pa, pb, len64, MemAttrs { src_align: 3, ..MemAttrs::aligned(3) });
            let zero32 = b.iconst(Ty::I32, 0);
            let is_eq = b.icmp(IntCond::Eq, cmp_eq, zero32);

            // Set byte 0 of buf_a = 10, byte 0 of buf_b = 20 => memcmp(a, b) < 0 and memcmp(b, a) > 0
            let byte_lo = b.iconst(Ty::I8, 10);
            let byte_hi = b.iconst(Ty::I8, 20);
            b.store(Flags::NONE, byte_lo, pa, 0, MemAttrs::aligned(0));
            b.store(Flags::NONE, byte_hi, pb, 0, MemAttrs::aligned(0));

            let cmp_lt = b.memcmp(pa, pb, len64, MemAttrs { src_align: 3, ..MemAttrs::aligned(3) });
            let cmp_gt = b.memcmp(pb, pa, len64, MemAttrs { src_align: 3, ..MemAttrs::aligned(3) });
            let is_lt = b.icmp(IntCond::Slt, cmp_lt, zero32);
            let is_gt = b.icmp(IntCond::Sgt, cmp_gt, zero32);

            let loaded_1 = b.load(Ty::I64, Flags::NONE, pb, 8, MemAttrs::aligned(3));
            let loaded_2 = b.load(Ty::I64, Flags::NONE, pb, 16, MemAttrs::aligned(3));
            let eq64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, is_eq);
            let lt64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, is_lt);
            let gt64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, is_gt);

            let two = b.iconst(Ty::I64, 2);
            let four = b.iconst(Ty::I64, 4);
            let lt_bit = b.binary(Op::Mul, Flags::NONE, lt64, two);
            let gt_bit = b.binary(Op::Mul, Flags::NONE, gt64, four);
            let flags = b.binary(Op::Or, Flags::NONE, eq64, lt_bit);
            let flags = b.binary(Op::Or, Flags::NONE, flags, gt_bit);
            let mix = b.binary(Op::Add, Flags::NONE, loaded_1, loaded_2);
            let out = b.binary(Op::Xor, Flags::NONE, mix, flags);
            b.ret(&[out]);
            b.finish_checked().expect("stack_ops builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "stack_ops", export_name: Some("stack_ops"), place: 1, view: stack_ops.view() },
        ];
        // Tight 8 KiB stack top: 200 calls * 128-byte frame = 25,600 bytes > 8 KiB.
        // Passing 200 consecutive calls proves `$__stack_pointer` is restored on every return.
        let opts = Wasm64Opts {
            min_pages: 4,
            max_pages: Some(16),
            stack_top: 8 * 1024,
            heap_base: 8 * 1024,
        };
        let module = LirWasm64Module {
            name: "oracle_shadow_stack",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts,
            profile_digest: None,
        };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);
        for iter in 0i64..200 {
            let seed = iter.wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64) ^ 0x5A;
            let got = inst.call_i64_1("stack_ops", seed).expect("stack_ops runs without leaking SP");
            let want = eval_single_i64(&stack_ops, &[Val::Int(seed as u64 as u128)]);
            assert_eq!(got, want, "stack_ops iter={iter} seed={seed:#x} mismatch");
        }
    }

    // ---- 5. `Switch` jump table (multiple cases + default) ----------------------------------

    #[test]
    fn oracle_switch_jump_table() {
        let switch_fn = {
            let mut b = begin_fn(&[Ty::I32], &[Ty::I32]);
            let b0 = b.create_block(BlockKind::Normal);
            let b1 = b.create_block(BlockKind::Normal);
            let b2 = b.create_block(BlockKind::Normal);
            let b7 = b.create_block(BlockKind::Normal);
            let b42 = b.create_block(BlockKind::Normal);
            let bdef = b.create_block(BlockKind::Normal);
            let merge = b.create_block(BlockKind::Normal);

            let x = b.param(Ty::I32);
            b.switch(
                x,
                (bdef, &[]),
                &[(0, (b0, &[])), (1, (b1, &[])), (2, (b2, &[])), (7, (b7, &[])), (42, (b42, &[]))],
            );

            for (blk, val) in [(b0, 100u128), (b1, 205), (b2, 310), (b7, 777), (b42, 4242)] {
                b.begin(blk);
                let c = b.iconst(Ty::I32, val);
                b.jump((merge, &[c]));
            }

            b.begin(bdef);
            let neg_one = b.iconst(Ty::I32, (-1i32) as u32 as u128);
            let shifted = b.binary(Op::Xor, Flags::NONE, x, neg_one);
            b.jump((merge, &[shifted]));

            b.begin(merge);
            let res = b.param(Ty::I32);
            b.ret(&[res]);
            b.finish_checked().expect("switch_fn builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "switch_dispatch", export_name: Some("switch_dispatch"), place: 1, view: switch_fn.view() },
        ];
        let module = LirWasm64Module {
            name: "oracle_switch",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);
        for x in [-1i32, 0, 1, 2, 3, 6, 7, 8, 41, 42, 43, 1000] {
            let got = inst.call_i32_1("switch_dispatch", x).expect("switch_dispatch runs");
            let want = eval_single_i32(&switch_fn, &[Val::Int(x as u32 as u128)]);
            assert_eq!(got, want, "switch_dispatch({x}) mismatch");
        }
    }

    // ---- 6. `FuncAddr` + `CallIndirect` (table 0 dispatch + null-pointer trap control) ------

    #[test]
    fn oracle_func_addr_and_call_indirect_with_null_trap_control() {
        const PLACE_DOUBLE: u32 = 1;
        const PLACE_SQUARE: u32 = 2;
        const PLACE_INC7: u32 = 3;
        const PLACE_DISPATCH: u32 = 4;

        let op_double = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let x = b.param(Ty::I64);
            let two = b.iconst(Ty::I64, 2);
            let out = b.binary(Op::Mul, Flags::NONE, x, two);
            b.ret(&[out]);
            b.finish_checked().expect("op_double builds")
        };

        let op_square = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let x = b.param(Ty::I64);
            let out = b.binary(Op::Mul, Flags::NONE, x, x);
            b.ret(&[out]);
            b.finish_checked().expect("op_square builds")
        };

        let op_inc7 = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let x = b.param(Ty::I64);
            let seven = b.iconst(Ty::I64, 7);
            let out = b.binary(Op::Add, Flags::NONE, x, seven);
            b.ret(&[out]);
            b.finish_checked().expect("op_inc7 builds")
        };

        let dispatch = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I64]);
            let unary_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let r_double = b.reference(RefKind::Func, PLACE_DOUBLE);
            let r_square = b.reference(RefKind::Func, PLACE_SQUARE);
            let r_inc7 = b.reference(RefKind::Func, PLACE_INC7);

            let which = b.param(Ty::I64);
            let x = b.param(Ty::I64);
            let f0 = b.func_addr(r_double);
            let f1 = b.func_addr(r_square);
            let f2 = b.func_addr(r_inc7);
            let fnull = b.null();

            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c2 = b.iconst(Ty::I64, 2);
            let is0 = b.icmp(IntCond::Eq, which, c0);
            let is1 = b.icmp(IntCond::Eq, which, c1);
            let is2 = b.icmp(IntCond::Eq, which, c2);

            let p = b.select(Flags::NONE, is2, f2, fnull);
            let p = b.select(Flags::NONE, is1, f1, p);
            let p = b.select(Flags::NONE, is0, f0, p);
            let res = b.call_indirect(Flags::NONE, p, unary_sig, &[x]);
            b.ret(&[res]);
            b.finish_checked().expect("dispatch builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "op_double", export_name: None, place: PLACE_DOUBLE, view: op_double.view() },
            LirWasm64Func { name: "op_square", export_name: None, place: PLACE_SQUARE, view: op_square.view() },
            LirWasm64Func { name: "op_inc7", export_name: None, place: PLACE_INC7, view: op_inc7.view() },
            LirWasm64Func { name: "dispatch", export_name: Some("dispatch"), place: PLACE_DISPATCH, view: dispatch.view() },
        ];
        let module = LirWasm64Module {
            name: "oracle_call_indirect",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let table_funcs = [
            (PLACE_DOUBLE, op_double.view()),
            (PLACE_SQUARE, op_square.view()),
            (PLACE_INC7, op_inc7.view()),
            (PLACE_DISPATCH, dispatch.view()),
        ];
        let bodies = TableBodies { funcs: &table_funcs, statics: &[] };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);
        for which in 0i64..=2 {
            for x in [-9i64, 0, 5, 13] {
                let got = inst.call_i64_2("dispatch", which, x).expect("call_indirect succeeds");
                let outcome = run_calls_in(
                    dispatch.view(),
                    body_id_for_place(PLACE_DISPATCH),
                    &[Val::Int(which as u64 as u128), Val::Int(x as u64 as u128)],
                    EVAL_FUEL,
                    &bodies,
                    test_alloc(),
                )
                .expect("eval runs call_indirect");
                let want = match outcome {
                    Outcome::Returned(vals) => match vals.as_slice() {
                        [Val::Int(bits)] => (*bits as u64) as i64,
                        other => panic!("unexpected eval return {other:?}"),
                    },
                    other => panic!("unexpected eval outcome {other:?}"),
                };
                assert_eq!(got, want, "dispatch({which}, {x}) mismatch");
            }
        }

        // Control: calling through null (table slot 0) traps cleanly in Wasmtime and is Undefined in eval.
        let null_err = inst
            .call_i64_2("dispatch", 3, 99)
            .expect_err("null function pointer call_indirect must trap");
        assert!(
            matches!(null_err, Wasm64RunError::Trap { .. }),
            "expected Trap on null call_indirect, got {null_err:?}"
        );
        let eval_null = run_calls_in(
            dispatch.view(),
            body_id_for_place(PLACE_DISPATCH),
            &[Val::Int(3), Val::Int(99)],
            EVAL_FUEL,
            &bodies,
            test_alloc(),
        )
        .expect("eval classifies null call_indirect");
        assert!(
            matches!(eval_null, Outcome::Undefined { .. }),
            "expected Undefined in eval for null call_indirect, got {eval_null:?}"
        );
    }

    // ---- 7. Static rodata (`LirWasm64Static` + `DataAddr` + `Load`) + `blaze_host_v1::host_log`

    #[test]
    fn oracle_static_rodata_and_host_log_from_memory64() {
        const PLACE_LOG_IMPORT: u32 = 10;
        const PLACE_GREETING_STATIC: u32 = 20;
        const PLACE_CHECKSUM: u32 = 1;
        const PLACE_LOG_FN: u32 = 2;
        const PLACE_PTR_FN: u32 = 3;
        const MSG: &[u8] = b"hello from wasm64 lir!";

        // Sums all bytes of the static rodata via DataAddr + Load (cross-checked against eval).
        let checksum_fn = {
            let mut b = begin_fn(&[], &[Ty::I64]);
            let hdr = b.create_block(BlockKind::Normal);
            let body = b.create_block(BlockKind::Normal);
            let done = b.create_block(BlockKind::Normal);

            let stat_ref = b.reference(RefKind::Static, PLACE_GREETING_STATIC);
            let base = b.data_addr(stat_ref);
            let zero = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let len = b.iconst(Ty::I64, MSG.len() as u128);
            b.jump((hdr, &[zero, zero]));

            b.begin(hdr);
            let idx = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let more = b.icmp(IntCond::Ult, idx, len);
            b.br(Flags::NONE, more, (body, &[]), (done, &[acc]));

            b.begin(body);
            let p = b.ptr_add(Flags::NONE, base, idx);
            let byte = b.load(Ty::I8, Flags::NONE, p, 0, MemAttrs::aligned(0));
            let byte64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, byte);
            let next_acc = b.binary(Op::Add, Flags::NONE, acc, byte64);
            let next_idx = b.binary(Op::Add, Flags::NONE, idx, one);
            b.jump((hdr, &[next_idx, next_acc]));

            b.begin(done);
            let total = b.param(Ty::I64);
            b.ret(&[total]);
            b.finish_checked().expect("checksum_fn builds")
        };

        // Calls `blaze_host_v1::host_log(2, DataAddr(greeting), MSG.len())`
        let log_fn = {
            let mut b = begin_fn(&[], &[Ty::I32]);
            let log_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I32), Param::of(Ty::PTR), Param::of(Ty::I32)],
                &[],
            );
            let log_ref = b.reference(RefKind::Func, PLACE_LOG_IMPORT);
            let stat_ref = b.reference(RefKind::Static, PLACE_GREETING_STATIC);
            let ptr = b.data_addr(stat_ref);
            let level = b.iconst(Ty::I32, 2);
            let len = b.iconst(Ty::I32, MSG.len() as u128);
            let _ = b.call(Flags::NONE, log_ref, log_sig, &[level, ptr, len]);
            let ok = b.iconst(Ty::I32, 1);
            b.ret(&[ok]);
            b.finish_checked().expect("log_fn builds")
        };

        // Returns raw 64-bit guest linear address of the static rodata so host can verify `read_memory`.
        let ptr_fn = {
            let mut b = begin_fn(&[], &[Ty::I64]);
            let stat_ref = b.reference(RefKind::Static, PLACE_GREETING_STATIC);
            let ptr = b.data_addr(stat_ref);
            let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, ptr);
            b.ret(&[addr]);
            b.finish_checked().expect("ptr_fn builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "checksum", export_name: Some("checksum"), place: PLACE_CHECKSUM, view: checksum_fn.view() },
            LirWasm64Func { name: "emit_log", export_name: Some("emit_log"), place: PLACE_LOG_FN, view: log_fn.view() },
            LirWasm64Func { name: "static_ptr", export_name: Some("static_ptr"), place: PLACE_PTR_FN, view: ptr_fn.view() },
        ];
        let statics = [LirWasm64Static { place: PLACE_GREETING_STATIC, align: 1, bytes: MSG, relocs: &[] }];
        let imports = [LirWasm64Import { place: PLACE_LOG_IMPORT, module: EXPORT_HOST_MODULE, field: "host_log", params: &[], results: &[] }];
        let module = LirWasm64Module {
            name: "oracle_rodata_and_host_log",
            funcs: &funcs,
            statics: &statics,
            imports: &imports,
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);

        // Cross-check static rodata byte checksum against blaze_lir::eval.
        let got_sum = inst.call_i64_0("checksum").expect("checksum runs");
        let table_funcs = [(PLACE_CHECKSUM, checksum_fn.view())];
        let table_statics = [(PLACE_GREETING_STATIC, MSG, 0u8)];
        let bodies = TableBodies { funcs: &table_funcs, statics: &table_statics };
        let eval_outcome = run_calls_in(
            checksum_fn.view(),
            body_id_for_place(PLACE_CHECKSUM),
            &[],
            EVAL_FUEL,
            &bodies,
            test_alloc(),
        )
        .expect("eval runs static checksum");
        let want_sum = match eval_outcome {
            Outcome::Returned(vals) => match vals.as_slice() {
                [Val::Int(bits)] => (*bits as u64) as i64,
                other => panic!("unexpected eval return {other:?}"),
            },
            other => panic!("unexpected eval outcome {other:?}"),
        };
        assert_eq!(got_sum, want_sum);
        assert_eq!(got_sum, MSG.iter().map(|&byte| i64::from(byte)).sum::<i64>());

        // Call host_log from LIR and assert host captured the exact UTF-8 string from memory64.
        assert_eq!(inst.call_i32_0("emit_log").expect("emit_log runs"), 1);
        let logs: std::vec::Vec<_> = inst.logs().collect();
        assert_eq!(logs, [(2, "hello from wasm64 lir!")]);

        // Inspect guest memory64 directly via `read_memory`.
        let guest_addr = inst.call_i64_0("static_ptr").expect("static_ptr runs");
        assert!(guest_addr >= 4096, "static rodata must sit above the null guard page, got {guest_addr}");
        let raw = inst
            .read_memory(guest_addr as u64, MSG.len())
            .expect("read_memory succeeds");
        assert_eq!(raw, MSG);
    }

    // ---- 8. Overflow arithmetic (`UaddOverflow` / `SaddOverflow` + `Project`) & `F64` -------

    #[test]
    fn oracle_overflow_arithmetic_and_f64_ops() {
        // Combines sum and overflow bit via `sum ^ ((overflow as i64) << 63)` so both sum and flag are checked.
        let uadd_ov = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I64]);
            let a = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let pair = b.overflow(Op::UaddOverflow, a, y);
            let sum = b.project(pair, 0);
            let ov = b.project(pair, 1);
            let ov64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, ov);
            let shift63 = b.iconst(Ty::I64, 63);
            let high_bit = b.binary(Op::Shl, Flags::NONE, ov64, shift63);
            let out = b.binary(Op::Xor, Flags::NONE, sum, high_bit);
            b.ret(&[out]);
            b.finish_checked().expect("uadd_ov builds")
        };

        let sadd_ov = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I64]);
            let a = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let pair = b.overflow(Op::SaddOverflow, a, y);
            let sum = b.project(pair, 0);
            let ov = b.project(pair, 1);
            let ov64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, ov);
            let shift63 = b.iconst(Ty::I64, 63);
            let high_bit = b.binary(Op::Shl, Flags::NONE, ov64, shift63);
            let out = b.binary(Op::Xor, Flags::NONE, sum, high_bit);
            b.ret(&[out]);
            b.finish_checked().expect("sadd_ov builds")
        };

        // hypot_calc(a, b) = sqrt(a * a + b * b)
        let f64_hypot = {
            let mut b = begin_fn(&[Ty::F64, Ty::F64], &[Ty::F64]);
            let a = b.param(Ty::F64);
            let y = b.param(Ty::F64);
            let a2 = b.binary(Op::Fmul, Flags::NONE, a, a);
            let b2 = b.binary(Op::Fmul, Flags::NONE, y, y);
            let sum = b.binary(Op::Fadd, Flags::NONE, a2, b2);
            let root = b.unary(Op::Sqrt, Flags::NONE, sum);
            b.ret(&[root]);
            b.finish_checked().expect("f64_hypot builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "uadd_ov", export_name: Some("uadd_ov"), place: 1, view: uadd_ov.view() },
            LirWasm64Func { name: "sadd_ov", export_name: Some("sadd_ov"), place: 2, view: sadd_ov.view() },
            LirWasm64Func { name: "f64_hypot", export_name: Some("f64_hypot"), place: 3, view: f64_hypot.view() },
        ];
        let module = LirWasm64Module {
            name: "oracle_overflow_and_f64",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let (mut inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);

        let ov_cases: &[(i64, i64)] = &[
            (0, 0),
            (100, 200),
            (-1, 0),
            (-1, 1),
            (i64::MAX, 0),
            (i64::MAX, 1),
            (i64::MIN, -1),
            (i64::MIN, i64::MIN),
        ];
        for &(a, b) in ov_cases {
            let got_u = inst.call_i64_2("uadd_ov", a, b).expect("uadd_ov runs");
            let want_u = eval_single_i64(&uadd_ov, &[Val::Int(a as u64 as u128), Val::Int(b as u64 as u128)]);
            assert_eq!(got_u, want_u, "uadd_ov({a:#x}, {b:#x}) mismatch");

            let got_s = inst.call_i64_2("sadd_ov", a, b).expect("sadd_ov runs");
            let want_s = eval_single_i64(&sadd_ov, &[Val::Int(a as u64 as u128), Val::Int(b as u64 as u128)]);
            assert_eq!(got_s, want_s, "sadd_ov({a:#x}, {b:#x}) mismatch");
        }

        let f64_cases: &[(f64, f64)] = &[
            (3.0, 4.0),
            (5.0, 12.0),
            (0.0, 0.0),
            (-3.0, 4.0),
            (1.5, 2.5),
            (0.1, 0.2),
        ];
        for &(a, b) in f64_cases {
            let got = inst.call_f64_2("f64_hypot", a, b).expect("f64_hypot runs");
            let want_bits = eval_single_f64_bits(
                &f64_hypot,
                &[Val::Int(u128::from(a.to_bits())), Val::Int(u128::from(b.to_bits()))],
            );
            assert_eq!(got.to_bits(), want_bits, "f64_hypot({a}, {b}) bit mismatch");
        }
    }

    // ---- 9. Sandbox enforcement: fuel exhaustion + `Op::Trap` + host::Settings memory64 -----

    #[test]
    fn oracle_sandbox_fuel_exhaustion_and_trap() {
        let inf_loop = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let loop_blk = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);
            let x = b.param(Ty::I64);
            let one = b.iconst(Ty::I64, 1);
            b.jump((loop_blk, &[x]));

            b.begin(loop_blk);
            let acc = b.param(Ty::I64);
            let next = b.binary(Op::Add, Flags::NONE, acc, one);
            // `acc.wrapping_add(1) == acc` is always false for every i64: guaranteed infinite loop.
            let never = b.icmp(IntCond::Eq, next, acc);
            b.br(Flags::NONE, never, (exit, &[next]), (loop_blk, &[next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().expect("inf_loop builds")
        };

        let trap_on_neg = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let fail = b.create_block(BlockKind::Normal);
            let ok = b.create_block(BlockKind::Normal);
            let x = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let is_neg = b.icmp(IntCond::Slt, x, zero);
            b.br(Flags::NONE, is_neg, (fail, &[]), (ok, &[]));

            b.begin(fail);
            b.trap(7);

            b.begin(ok);
            b.ret(&[x]);
            b.finish_checked().expect("trap_on_neg builds")
        };

        let start_fn = build_empty_start();
        let funcs = [
            LirWasm64Func { name: "_start", export_name: Some("_start"), place: 0, view: start_fn.view() },
            LirWasm64Func { name: "inf_loop", export_name: Some("inf_loop"), place: 1, view: inf_loop.view() },
            LirWasm64Func { name: "trap_on_neg", export_name: Some("trap_on_neg"), place: 2, view: trap_on_neg.view() },
        ];
        let module = LirWasm64Module {
            name: "oracle_sandbox_limits",
            funcs: &funcs,
            statics: &[],
            imports: &[],
            globals: &[],
            include_blaze_host_v1: true,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        // 1. Infinite loop deterministically hits OutOfFuel in Wasmtime and in blaze_lir::eval.
        let fuel_limit = 50_000;
        let limits = Wasm64Limits { fuel: fuel_limit, ..Wasm64Limits::STANDARD };
        let (mut inst, wasm) = compile_and_instantiate(&module, limits);
        let fuel_err = inst
            .call_i64_1("inf_loop", 0)
            .expect_err("infinite loop must exhaust fuel");
        assert!(
            matches!(fuel_err, Wasm64RunError::OutOfFuel { fuel_limit: got } if got == fuel_limit),
            "expected OutOfFuel({fuel_limit}), got {fuel_err:?}"
        );
        assert_eq!(inst.fuel_remaining(), 0);
        let eval_fuel = run_in(inf_loop.view(), &[Val::Int(0)], 500, test_alloc()).expect("eval runs");
        assert!(matches!(eval_fuel, Outcome::OutOfFuel), "expected eval OutOfFuel, got {eval_fuel:?}");

        // 2. Op::Trap traps cleanly in Wasmtime on negative input and passes on non-negative input.
        let (mut trap_inst, _) = compile_and_instantiate(&module, Wasm64Limits::STANDARD);
        assert_eq!(trap_inst.call_i64_1("trap_on_neg", 42).expect("non-negative passes"), 42);
        let trap_err = trap_inst
            .call_i64_1("trap_on_neg", -1)
            .expect_err("negative input must hit Op::Trap");
        assert!(
            matches!(trap_err, Wasm64RunError::Trap { .. }),
            "expected Wasm64RunError::Trap, got {trap_err:?}"
        );
        let eval_trap = run_in(
            trap_on_neg.view(),
            &[Val::Int((-1i64) as u64 as u128)],
            EVAL_FUEL,
            test_alloc(),
        )
        .expect("eval runs trap_on_neg");
        assert_eq!(eval_trap, Outcome::<Alloc<'static>>::Trapped(EvalTrap::Code(7)));

        // 3. Verify `crate::host::Settings::standard().config()` also accepts memory64 modules.
        let host_engine = wasmtime::Engine::new(&crate::host::Settings::standard().config())
            .expect("host::Settings engine initializes with memory64");
        wasmtime::Module::new(&host_engine, &wasm)
            .expect("host::Settings engine compiles memory64 module");
    }

    // ---- 10. End-to-end `compile_lir_wasm64` -> `ScriptRuntime` (`wasi_snapshot_preview1_64`)

    #[test]
    fn wasm64_build_script_runs_end_to_end_through_script_runtime() {
        use std::sync::Arc;
        use blaze_build_script::{Env, key::{CMode, RunLimits}};
        use blaze_export::{
            lir_wasm64::{LirWasm64Global, LirWasm64GlobalValue},
            wasm64::ValType,
        };
        use crate::{
            script::ScriptRuntime,
            script_sandbox::{Script, ScriptRoot, Tree},
        };

        const PLACE_START: u32 = 0;
        const PLACE_MSG: u32 = 10;
        const PLACE_FD_WRITE: u32 = 20;
        const PLACE_PROC_EXIT: u32 = 21;
        const PLACE_MEM_GROW: u32 = 22;
        const PLACE_HEAP_BASE: u32 = 23;

        const CARGO_LINES: &[u8] = b"cargo:rustc-cfg=from_wasm64\ncargo:rustc-env=WASM64_KEY=hello64\n";

        let start_fn = {
            let mut b = begin_fn(&[], &[]);
            let grow_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let heap_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
            let fd_write_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I32), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I32)],
            );
            let exit_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[]);

            let r_grow = b.reference(RefKind::Func, PLACE_MEM_GROW);
            let r_heap = b.reference(RefKind::Func, PLACE_HEAP_BASE);
            let r_fd_write = b.reference(RefKind::Func, PLACE_FD_WRITE);
            let r_exit = b.reference(RefKind::Func, PLACE_PROC_EXIT);
            let r_msg = b.reference(RefKind::Static, PLACE_MSG);

            // Grow memory by 1 page via inline `__blaze_wasm64::memory_grow`, then use `heap_base`
            // for our 16-byte 64-bit WASI iovec `[ptr: u64, len: u64]` + 8-byte `nwritten`.
            let one_page = b.iconst(Ty::I64, 1);
            let _prev = b.call(Flags::NONE, r_grow, grow_sig, &[one_page]);
            let base = b.call(Flags::NONE, r_heap, heap_sig, &[]);
            let base_ptr = b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, base);

            let msg_ptr = b.data_addr(r_msg);
            let msg_addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, msg_ptr);
            let msg_len = b.iconst(Ty::I64, CARGO_LINES.len() as u128);
            let off16 = b.iconst(Ty::I64, 16);
            let nwritten_addr = b.binary(Op::Add, Flags::NONE, base, off16);

            let ma = MemAttrs::aligned(3);
            b.store(Flags::NONE, msg_addr, base_ptr, 0, ma);
            b.store(Flags::NONE, msg_len, base_ptr, 8, ma);

            let stdout_fd = b.iconst(Ty::I32, 1);
            let one_iov = b.iconst(Ty::I64, 1);
            let _errno = b.call(Flags::NONE, r_fd_write, fd_write_sig, &[stdout_fd, base, one_iov, nwritten_addr]);

            let zero = b.iconst(Ty::I32, 0);
            let _ = b.call(Flags::NONE, r_exit, exit_sig, &[zero]);
            b.ret(&[]);
            b.finish_checked().expect("wasm64 build.rs _start builds")
        };

        let funcs = [LirWasm64Func { name: "_start", export_name: Some("_start"), place: PLACE_START, view: start_fn.view() }];
        let statics = [LirWasm64Static { place: PLACE_MSG, align: 1, bytes: CARGO_LINES, relocs: &[] }];
        let imports = [
            LirWasm64Import {
                place: PLACE_FD_WRITE,
                module: "wasi_snapshot_preview1_64",
                field: "fd_write",
                params: &[ValType::I32, ValType::I64, ValType::I64, ValType::I64],
                results: &[ValType::I32],
            },
            LirWasm64Import {
                place: PLACE_PROC_EXIT,
                module: "wasi_snapshot_preview1_64",
                field: "proc_exit",
                params: &[ValType::I32],
                results: &[],
            },
            LirWasm64Import {
                place: PLACE_MEM_GROW,
                module: "__blaze_wasm64",
                field: "memory_grow",
                params: &[ValType::I64],
                results: &[ValType::I64],
            },
            LirWasm64Import {
                place: PLACE_HEAP_BASE,
                module: "__blaze_wasm64",
                field: "heap_base",
                params: &[],
                results: &[ValType::I64],
            },
        ];
        let globals = [LirWasm64Global { name: "__test_msg_addr", value: LirWasm64GlobalValue::StaticAddr(PLACE_MSG) }];
        let module = LirWasm64Module {
            name: "wasm64_build_script_e2e",
            funcs: &funcs,
            statics: &statics,
            imports: &imports,
            globals: &globals,
            include_blaze_host_v1: false,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let alloc = test_alloc();
        let mut wasm = AVec::new_in(alloc);
        compile_lir_wasm64(&module, &mut wasm, alloc).expect("compile_lir_wasm64 succeeds");
        let checked = check_wasm64(&wasm).expect("check_wasm64 validates WASI-64 build script");
        assert!(checked.memory64);
        assert!(!checked.has_blaze_host_v1);
        assert_eq!(checked.import_count, 2, "only fd_write and proc_exit imported; memory intrinsics inline");

        let dir = std::env::temp_dir().join(format!("blaze-wasm64-script-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let rt = ScriptRuntime::new(&dir).expect("ScriptRuntime initializes");
        let loaded = rt.load("wasm64-demo 0.1.0", &wasm).expect("ScriptRuntime loads wasm64 module");

        let vars = vec![("OUT_DIR".to_owned(), "/out".to_owned())];
        let mut env_vars = blaze_own::Vec::new_in(alloc);
        env_vars.push((blaze_own::String::from_str_in("OUT_DIR", alloc), blaze_own::String::from_str_in("/out", alloc)));
        let env = Env { vars: env_vars };
        let script = Arc::new(Script {
            package: "wasm64-demo 0.1.0".to_owned(),
            tree: Tree::Mem { files: Arc::new(Vec::new()), submodules: Arc::from([]), git: None },
            root: ScriptRoot::package(),
            env: vars,
            target: "aarch64-apple-darwin".to_owned(),
            host: "aarch64-apple-darwin".to_owned(),
            c_mode: CMode::Deferred,
            limits: RunLimits { fuel: 1 << 26, memory: 16 << 20, out_dir_bytes: 1 << 20, out_dir_files: 16, output: 1 << 20, line: 256 << 10 },
            table: Arc::new(Vec::new()),
            probes: None,
            c_probes: None,
            spawner: None,
            step: blaze_ids::ContentId::default(),
            spawn_setting: Arc::new(crate::spawn_route::SpawnSetting::off()),
            deps: Arc::new(crate::script_sandbox::dep_mount::DepViews::none()),
        });

        let ran = rt.run(&loaded, script, &env, alloc).expect("wasm64 build script runs cleanly");
        assert!(ran.fuel > 0, "fuel must be metered on wasm64 build script");
        assert!(
            ran.outputs.cfgs.iter().any(|(name, value)| name.as_str() == "from_wasm64" && value.is_none()),
            "expected cargo:rustc-cfg=from_wasm64 in {:?}",
            ran.outputs.cfgs
        );
        assert!(
            ran.outputs.env.iter().any(|(name, value)| name.as_str() == "WASM64_KEY" && value.as_str() == "hello64"),
            "expected cargo:rustc-env=WASM64_KEY=hello64 in {:?}",
            ran.outputs.env
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 11. End-to-end `compile_lir_wasm64` -> `host::Runtime` (`blaze_proc_macro_64`) ------

    #[test]
    fn wasm64_proc_macro_runs_end_to_end_through_host_runtime() {
        use std::sync::Arc;
        use blaze_expand::{Edition, ProcCall, ProcKind, lex};
        use blaze_export::{
            lir_wasm64::{LirWasm64Global, LirWasm64GlobalValue, LirWasm64RelocTarget, LirWasm64StaticReloc},
            wasm64::ValType,
        };
        use crate::{
            host::{CallError, Limits, Runtime},
            module::{Exported, ModuleRecord, sha256_hex},
            package_env::PackageEnv,
            sandbox::{Files, Package},
        };

        const PLACE_INIT: u32 = 0;
        const PLACE_ALLOC: u32 = 1;
        const PLACE_EXPAND: u32 = 2;
        const PLACE_MACRO_IMPL: u32 = 3;

        const PLACE_DECLS: u32 = 10;
        const PLACE_ENV_KEY: u32 = 11;
        const PLACE_EMPTY_REPLY: u32 = 12;

        const PLACE_ENV_VAR: u32 = 20;
        const PLACE_OUTPUT: u32 = 21;
        const PLACE_HEAP_BASE: u32 = 22;

        const ENV_KEY: &[u8] = b"FOO";
        // Bridge encoding of `Ok(None)` (empty token stream): `0u8` (`Ok`) + `1u8` (`None`).
        const EMPTY_REPLY: &[u8] = &[0u8, 1u8];
        const DECLS_ZERO: &[u8] = &[0u8; 8];

        let init_fn = {
            let mut b = begin_fn(&[], &[]);
            b.ret(&[]);
            b.finish_checked().expect("_initialize builds")
        };

        let alloc_fn = {
            let mut b = begin_fn(&[Ty::I64], &[Ty::I64]);
            let _len = b.param(Ty::I64);
            let heap_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
            let r_heap = b.reference(RefKind::Func, PLACE_HEAP_BASE);
            let base = b.call(Flags::NONE, r_heap, heap_sig, &[]);
            b.ret(&[base]);
            b.finish_checked().expect("blaze_proc_macro_alloc builds")
        };

        let macro_impl = {
            let mut b = begin_fn(&[Ty::I64, Ty::I64], &[Ty::I32]);
            let _in_ptr = b.param(Ty::I64);
            let _in_len = b.param(Ty::I64);
            let env_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            let out_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[]);
            let heap_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);

            let r_env = b.reference(RefKind::Func, PLACE_ENV_VAR);
            let r_out = b.reference(RefKind::Func, PLACE_OUTPUT);
            let r_heap = b.reference(RefKind::Func, PLACE_HEAP_BASE);
            let r_key = b.reference(RefKind::Static, PLACE_ENV_KEY);
            let r_reply = b.reference(RefKind::Static, PLACE_EMPTY_REPLY);

            // Read env var `FOO` into scratch buffer above input (heap_base + 4096) so Sandbox tracks it.
            let base = b.call(Flags::NONE, r_heap, heap_sig, &[]);
            let off = b.iconst(Ty::I64, 4096);
            let buf = b.binary(Op::Add, Flags::NONE, base, off);
            let key_ptr = b.data_addr(r_key);
            let key_addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, key_ptr);
            let key_len = b.iconst(Ty::I64, ENV_KEY.len() as u128);
            let cap = b.iconst(Ty::I64, 64);
            let _env_len = b.call(Flags::NONE, r_env, env_sig, &[key_addr, key_len, buf, cap]);

            // Emit valid bridge empty token stream reply `[0, 0]`.
            let reply_ptr = b.data_addr(r_reply);
            let reply_addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, reply_ptr);
            let reply_len = b.iconst(Ty::I64, EMPTY_REPLY.len() as u128);
            let _ = b.call(Flags::NONE, r_out, out_sig, &[reply_addr, reply_len]);

            let ok = b.iconst(Ty::I32, 0);
            b.ret(&[ok]);
            b.finish_checked().expect("macro_impl builds")
        };

        let expand_fn = {
            let mut b = begin_fn(&[Ty::I64, Ty::I32, Ty::I64, Ty::I64], &[Ty::I32]);
            let ok_blk = b.create_block(BlockKind::Normal);
            let bad_blk = b.create_block(BlockKind::Normal);

            let decls = b.param(Ty::I64);
            let index = b.param(Ty::I32);
            let in_ptr = b.param(Ty::I64);
            let in_len = b.param(Ty::I64);

            let zero = b.iconst(Ty::I32, 0);
            let is_zero = b.icmp(IntCond::Eq, index, zero);
            b.br(Flags::NONE, is_zero, (ok_blk, &[]), (bad_blk, &[]));

            b.begin(ok_blk);
            let ma = MemAttrs::aligned(3);
            let decls_ptr = b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, decls);
            // Load patched function-table index from `decls[0]` and dispatch via `call_indirect`!
            let fn_idx = b.load(Ty::I64, Flags::NONE, decls_ptr, 0, ma);
            let fn_ptr = b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, fn_idx);
            let impl_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I32)]);
            let status = b.call_indirect(Flags::NONE, fn_ptr, impl_sig, &[in_ptr, in_len]);
            b.ret(&[status]);

            b.begin(bad_blk);
            let missing = b.iconst(Ty::I32, 1);
            b.ret(&[missing]);
            b.finish_checked().expect("blaze_proc_macro_expand builds")
        };

        let funcs = [
            LirWasm64Func { name: "_initialize", export_name: Some("_initialize"), place: PLACE_INIT, view: init_fn.view() },
            LirWasm64Func { name: "blaze_proc_macro_alloc", export_name: Some("blaze_proc_macro_alloc"), place: PLACE_ALLOC, view: alloc_fn.view() },
            LirWasm64Func { name: "blaze_proc_macro_expand", export_name: Some("blaze_proc_macro_expand"), place: PLACE_EXPAND, view: expand_fn.view() },
            LirWasm64Func { name: "macro_impl", export_name: None, place: PLACE_MACRO_IMPL, view: macro_impl.view() },
        ];
        let decls_relocs = [LirWasm64StaticReloc {
            offset: 0,
            target: LirWasm64RelocTarget::Func(PLACE_MACRO_IMPL),
            addend: 0,
        }];
        let statics = [
            LirWasm64Static { place: PLACE_DECLS, align: 8, bytes: DECLS_ZERO, relocs: &decls_relocs },
            LirWasm64Static { place: PLACE_ENV_KEY, align: 1, bytes: ENV_KEY, relocs: &[] },
            LirWasm64Static { place: PLACE_EMPTY_REPLY, align: 1, bytes: EMPTY_REPLY, relocs: &[] },
        ];
        let imports = [
            LirWasm64Import {
                place: PLACE_ENV_VAR,
                module: "blaze_proc_macro_64",
                field: "env_var",
                params: &[ValType::I64, ValType::I64, ValType::I64, ValType::I64],
                results: &[ValType::I64],
            },
            LirWasm64Import {
                place: PLACE_OUTPUT,
                module: "blaze_proc_macro_64",
                field: "output",
                params: &[ValType::I64, ValType::I64],
                results: &[],
            },
            LirWasm64Import {
                place: PLACE_HEAP_BASE,
                module: "__blaze_wasm64",
                field: "heap_base",
                params: &[],
                results: &[ValType::I64],
            },
        ];
        let globals = [LirWasm64Global {
            name: "__rustc_proc_macro_decls_wasm64__",
            value: LirWasm64GlobalValue::StaticAddr(PLACE_DECLS),
        }];
        let module = LirWasm64Module {
            name: "wasm64_proc_macro_e2e",
            funcs: &funcs,
            statics: &statics,
            imports: &imports,
            globals: &globals,
            include_blaze_host_v1: false,
            opts: Wasm64Opts::DEFAULT,
            profile_digest: None,
        };

        let alloc = test_alloc();
        let mut wasm = AVec::new_in(alloc);
        compile_lir_wasm64(&module, &mut wasm, alloc).expect("compile_lir_wasm64 succeeds");
        let checked = check_wasm64(&wasm).expect("check_wasm64 validates proc-macro reactor");
        assert!(checked.memory64);
        assert!(!checked.has_blaze_host_v1);
        assert_eq!(checked.import_count, 2);

        let dir = std::env::temp_dir().join(format!("blaze-wasm64-pm-e2e-{}", std::process::id()));
        let pkg_dir = dir.join("pkg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&pkg_dir).expect("pkg dir");
        std::fs::write(dir.join("module.wasm"), wasm.as_slice()).expect("write module.wasm");
        let digest = sha256_hex(wasm.as_slice());

        let record = ModuleRecord {
            key: digest.clone(),
            name: "wasm64-pm-demo".to_owned(),
            version: "0.1.0".to_owned(),
            wasm_sha256: digest,
            macros: vec![
                Exported { index: 0, kind: ProcKind::FnLike, name: "demo".to_owned(), helpers: Vec::new() },
                Exported { index: 1, kind: ProcKind::FnLike, name: "missing".to_owned(), helpers: Vec::new() },
            ],
            decls: "__rustc_proc_macro_decls_wasm64__".to_owned(),
            dir: dir.clone(),
            compiled: dir.join("compiled"),
            native_dylib: None,
        };

        let rt = Runtime::new(blaze_own::testing::heap()).expect("Runtime initializes");
        let loaded = rt.load(&record).expect("Runtime loads wasm64 proc-macro reactor");
        let env = PackageEnv::set_only(vec![("FOO".to_owned(), "bar64".to_owned())]).expect("PackageEnv");
        let pkg = Arc::new(Package::new(Files::Folder(pkg_dir), env).expect("Package"));

        let input = lex("struct Demo;", Edition::E2024, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        let ran = rt
            .expand(&loaded, &record.macros[0], &call, Edition::E2024, Arc::clone(&pkg), Limits::STANDARD, alloc)
            .expect("wasm64 proc-macro expands via indirect decls dispatch");
        assert!(ran.produced.tokens.is_empty());
        assert!(ran.fuel > 0);
        assert!(
            ran.produced.reads.list().iter().any(|r| ran.produced.reads.subject(r) == "env:FOO"),
            "expected tracked env:FOO read in {:?}",
            ran.produced.reads.list()
        );

        // Control: out-of-range macro index returns status 1 and is refused cleanly.
        let err = rt
            .expand(&loaded, &record.macros[1], &call, Edition::E2024, pkg, Limits::STANDARD, alloc)
            .err()
            .expect("index 1 must fail");
        assert!(
            matches!(err.error, CallError::Failed(ref why) if why.contains("no macro at index 1")),
            "expected missing macro index error, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
