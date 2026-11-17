//! One module through LLVM (D1, as changed by D100 points 7, 9 and 14): text IR in, a fresh
//! context per call, the named custom pipeline, the inliner's remarks read back, object bytes
//! (and, for [`bitcode`], the optimized module's bitcode) out, every LLVM diagnostic of error or
//! warning severity an error.
//!
//! Nothing is shared between calls but LLVM's target registry ([`init`]): no context, module,
//! target machine or pass manager outlives a call, so a function compiled alone and the same
//! function compiled in a process that compiled others first give equal bytes (tested).
//!
//! The target CPU is named by the caller and given to LLVM as named; `native` and the empty
//! name are refused (D100 point 14): a record's bytes never depend on the machine that made it.

use core::ffi::{c_char, c_void};
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicU8, Ordering};

use blaze_code::record::Symbol;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_tier::{Arch, Machine, Os};

use crate::ffi;
use crate::names::ir_name;
use crate::pipeline::Pipeline;
use crate::remarks::{self, INLINER_PASSES, Remark, RemarkError};

/// An LLVM diagnostic's severity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub const fn name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// Why a module was not compiled. Messages are LLVM's own, copied into the caller's allocator.
#[derive(Debug)]
pub enum LlvmError<'a> {
    /// [`init`] has not finished: no target is registered.
    NotInitialized,
    /// [`init`] was called while another thread was running it.
    InitRacing,
    /// The CPU is `native` or empty (D100 point 14), or holds a NUL byte.
    Cpu { what: &'static str },
    /// `machine.features` holds a bit this crate does not name for its arch.
    Features { arch: Arch, bits: u64 },
    /// The pipeline has no pass text yet (pipeline.rs `BLAZE_O2_PASSES` says why).
    NoPipeline { pipeline: &'static str },
    /// LLVM has no target for the triple.
    Target { triple: &'static str, message: AString<'a> },
    /// The IR text did not parse.
    Parse { message: AString<'a> },
    /// An offered callee's bitcode did not read, or is not one definition of its symbol.
    Bitcode { symbol: Symbol, what: &'static str, message: AString<'a> },
    /// Linking an offered callee's module into the caller's failed.
    Link { symbol: Symbol },
    /// The module (after linking) is not valid IR.
    Verify { message: AString<'a> },
    /// LLVM refused the pipeline's text or a pass failed.
    Passes { pipeline: &'static str, message: AString<'a> },
    /// LLVM reported a diagnostic of error or warning severity.
    Diagnostic { severity: Severity, message: AString<'a> },
    /// The inliner's remark stream could not be started or read (D100 point 7 (c)).
    Remarks { error: RemarkError },
    /// The remark stream could not be started on the context.
    RemarksStart { message: AString<'a> },
    /// The code generator failed.
    Emit { message: AString<'a> },
}

impl fmt::Display for LlvmError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlvmError::NotInitialized => f.write_str("blaze-llvm: `init` has not run: no LLVM target is registered"),
            LlvmError::InitRacing => f.write_str("blaze-llvm: `init` was called while another thread ran it"),
            LlvmError::Cpu { what } => write!(f, "blaze-llvm: the target CPU {what} (D100 point 14: name it)"),
            LlvmError::Features { arch, bits } => write!(f, "blaze-llvm: unknown feature bits {bits:#x} for {arch:?}"),
            LlvmError::NoPipeline { pipeline } => {
                write!(f, "blaze-llvm: the pipeline `{pipeline}` has no pass text yet (pipeline.rs BLAZE_O2_PASSES)")
            }
            LlvmError::Target { triple, message } => write!(f, "blaze-llvm: no LLVM target for `{triple}`: {message}"),
            LlvmError::Parse { message } => write!(f, "blaze-llvm: the IR did not parse: {message}"),
            LlvmError::Bitcode { symbol, what, message } => {
                write!(f, "blaze-llvm: the offered bitcode of {symbol}: {what}: {message}")
            }
            LlvmError::Link { symbol } => write!(f, "blaze-llvm: linking the offered body of {symbol} failed"),
            LlvmError::Verify { message } => write!(f, "blaze-llvm: the module is not valid IR: {message}"),
            LlvmError::Passes { pipeline, message } => write!(f, "blaze-llvm: the pipeline `{pipeline}`: {message}"),
            LlvmError::Diagnostic { severity, message } => {
                write!(f, "blaze-llvm: LLVM {}: {message}", severity.name())
            }
            LlvmError::Remarks { error } => write!(f, "blaze-llvm: the inliner's remark stream: {error}"),
            LlvmError::RemarksStart { message } => write!(f, "blaze-llvm: the remark stream did not start: {message}"),
            LlvmError::Emit { message } => write!(f, "blaze-llvm: the code generator: {message}"),
        }
    }
}

impl core::error::Error for LlvmError<'_> {}

/// An offered callee in its optimized bitcode (D100 point 9): linked into the module with its
/// body `available_externally`, so the inliner may splice it and nothing is emitted for it.
#[derive(Clone, Copy, Debug)]
pub struct Linked<'b> {
    pub symbol: Symbol,
    pub bytes: &'b [u8],
}

const UNINIT: u8 = 0;
const RUNNING: u8 = 1;
const READY: u8 = 2;

/// LLVM's target registry is the process's (LLVM's own global state); this says whether
/// blaze's targets are in it.
static TARGETS: AtomicU8 = AtomicU8::new(UNINIT);

/// Registers the arm64 and x86-64 targets with LLVM: once per process, before the first
/// compile, from one thread (the process's start). A second call after the first finished
/// does nothing; a call while another runs is `LlvmError::InitRacing` (it never waits).
pub fn init() -> Result<(), LlvmError<'static>> {
    match TARGETS.compare_exchange(UNINIT, RUNNING, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(READY) => return Ok(()),
        Err(_) => return Err(LlvmError::InitRacing),
    }
    // SAFETY: each is a target library's registration entry point, linked by llvm-sys (the
    // pinned prefix's lib/ has libLLVMAArch64*.a and libLLVMX86*.a). Registration writes
    // LLVM's global registry; the state above admits exactly one caller, and no compile runs
    // before READY is stored.
    unsafe {
        ffi::LLVMInitializeAArch64TargetInfo();
        ffi::LLVMInitializeAArch64Target();
        ffi::LLVMInitializeAArch64TargetMC();
        ffi::LLVMInitializeAArch64AsmPrinter();
        ffi::LLVMInitializeX86TargetInfo();
        ffi::LLVMInitializeX86Target();
        ffi::LLVMInitializeX86TargetMC();
        ffi::LLVMInitializeX86AsmPrinter();
    }
    TARGETS.store(READY, Ordering::Release);
    Ok(())
}

/// Compiles the text IR `ir` for `machine` and `cpu` through `pipeline`: the object file's bytes
/// are appended to `out`, the inliner's decisions to `remarks` (sorted, unique).
pub fn compile<'a>(
    ir: &[u8],
    machine: &Machine,
    cpu: &str,
    pipeline: Pipeline,
    out: &mut AVec<'a, u8>,
    remarks: &mut AVec<'a, Remark>,
    alloc: Alloc<'a>,
) -> Result<(), LlvmError<'a>> {
    run(ir, &[], machine, cpu, pipeline, out, None, remarks, alloc)
}

/// As [`compile`], with the offered callees' optimized bitcode linked in first, and the
/// optimized module's bitcode appended to `bitcode` (D100 point 9: kept as a record and offered
/// to this function's own callers). One optimization serves both outputs: the bitcode is written
/// after the pipeline and before the code generator, which changes the module.
#[allow(clippy::too_many_arguments)]
pub fn bitcode<'a>(
    ir: &[u8],
    offered: &[Linked<'_>],
    machine: &Machine,
    cpu: &str,
    pipeline: Pipeline,
    object: &mut AVec<'a, u8>,
    bitcode: &mut AVec<'a, u8>,
    remarks: &mut AVec<'a, Remark>,
    alloc: Alloc<'a>,
) -> Result<(), LlvmError<'a>> {
    run(ir, offered, machine, cpu, pipeline, object, Some(bitcode), remarks, alloc)
}

/// The triple of `machine`. The OS's deployment target is fixed per OS (macOS 11, the first
/// with arm64), so it is in the key through `machine.os`.
pub const fn triple(machine: &Machine) -> &'static str {
    match (machine.arch, machine.os) {
        (Arch::Arm64, Os::MacOs) => "arm64-apple-macosx11.0.0",
        (Arch::Arm64, Os::Linux) => "aarch64-unknown-linux-gnu",
        (Arch::X86_64, Os::MacOs) => "x86_64-apple-macosx11.0.0",
        (Arch::X86_64, Os::Linux) => "x86_64-unknown-linux-gnu",
    }
}

fn triple_c(machine: &Machine) -> &'static [u8] {
    match (machine.arch, machine.os) {
        (Arch::Arm64, Os::MacOs) => b"arm64-apple-macosx11.0.0\0",
        (Arch::Arm64, Os::Linux) => b"aarch64-unknown-linux-gnu\0",
        (Arch::X86_64, Os::MacOs) => b"x86_64-apple-macosx11.0.0\0",
        (Arch::X86_64, Os::Linux) => b"x86_64-unknown-linux-gnu\0",
    }
}

/// The feature string of `machine`, every named bit said either way so the CPU's defaults
/// decide nothing the key does not hold.
fn features_c<'a>(machine: &Machine) -> Result<&'static [u8], LlvmError<'a>> {
    match machine.arch {
        Arch::Arm64 => {
            let unknown = machine.features & !Machine::LSE;
            if unknown != 0 {
                return Err(LlvmError::Features { arch: machine.arch, bits: unknown });
            }
            Ok(if machine.features & Machine::LSE != 0 { b"+lse\0" } else { b"-lse\0" })
        }
        Arch::X86_64 => match machine.features {
            0 => Ok(b"\0"),
            bits => Err(LlvmError::Features { arch: machine.arch, bits }),
        },
    }
}

/// The first error or warning LLVM reported to the context's handler, kept in place (the
/// handler runs inside LLVM and allocates nothing; a longer message is cut at `MESSAGE`).
struct Diagnostics {
    severity: Option<Severity>,
    message: [u8; MESSAGE],
    len: usize,
}

const MESSAGE: usize = 1024;

extern "C" fn on_diagnostic(info: ffi::DiagnosticInfoRef, context: *mut c_void) {
    // SAFETY: `context` is the `Diagnostics` `run` registered with this context, alive and not
    // otherwise borrowed while LLVM runs (run reads it only between LLVM calls); `info` is live
    // for this call (Core.h), its description owned by us and freed here.
    unsafe {
        let diagnostics = &mut *context.cast::<Diagnostics>();
        let severity = match ffi::LLVMGetDiagInfoSeverity(info) {
            ffi::DS_ERROR => Severity::Error,
            ffi::DS_WARNING => Severity::Warning,
            // Remarks reach the remark stream, not this handler; a note belongs to the error
            // or warning before it, whose message LLVM prints whole.
            ffi::DS_REMARK | ffi::DS_NOTE => return,
            _ => Severity::Error,
        };
        if diagnostics.severity.is_some() {
            return;
        }
        let text = ffi::LLVMGetDiagInfoDescription(info);
        let bytes = ffi::c_bytes(text);
        let len = bytes.len().min(MESSAGE);
        diagnostics.message[..len].copy_from_slice(&bytes[..len]);
        diagnostics.len = len;
        diagnostics.severity = Some(severity);
        ffi::LLVMDisposeMessage(text);
    }
}

/// `bytes` as text in `alloc`: the longest valid UTF-8 prefix (LLVM's messages are ASCII).
fn text_in<'a>(bytes: &[u8], alloc: Alloc<'a>) -> AString<'a> {
    let valid = match core::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => core::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or(""),
    };
    let mut out = String::new_in(alloc);
    out.push_str(valid);
    out
}

/// A message LLVM allocated with `malloc`, copied into `alloc` and freed.
///
/// # Safety
///
/// `message` is null or a string LLVM handed out to be freed with `LLVMDisposeMessage`.
unsafe fn take_message<'a>(message: *mut c_char, alloc: Alloc<'a>) -> AString<'a> {
    // SAFETY: the caller's contract.
    unsafe {
        let out = text_in(ffi::c_bytes(message), alloc);
        if !message.is_null() {
            ffi::LLVMDisposeMessage(message);
        }
        out
    }
}

/// An `LLVMErrorRef`'s message, copied and the error consumed.
///
/// # Safety
///
/// `error` is a live, unconsumed, non-null error.
unsafe fn take_error<'a>(error: ffi::ErrorRef, alloc: Alloc<'a>) -> AString<'a> {
    // SAFETY: the caller's contract; `LLVMGetErrorMessage` consumes the error and its message
    // is freed with `LLVMDisposeErrorMessage` (Error.h).
    unsafe {
        let message = ffi::LLVMGetErrorMessage(error);
        let out = text_in(ffi::c_bytes(message), alloc);
        ffi::LLVMDisposeErrorMessage(message);
        out
    }
}

/// Owners of LLVM handles: each disposes its handle once, when dropped. Declared in `run` in
/// the order that drops a module before its context.
struct ContextOwner(ffi::ContextRef);
impl Drop for ContextOwner {
    fn drop(&mut self) {
        // SAFETY: created by `LLVMContextCreate`, disposed once; every module of it was disposed
        // before (declaration order in `run`).
        unsafe { ffi::LLVMContextDispose(self.0) }
    }
}

struct ModuleOwner(ffi::ModuleRef);
impl Drop for ModuleOwner {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a module this call parsed and still owns (null once given to the linker).
            unsafe { ffi::LLVMDisposeModule(self.0) }
        }
    }
}

struct BufferOwner(ffi::MemoryBufferRef);
impl Drop for BufferOwner {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a buffer this call created; no parser consumed it (IRReader.h's
            // `LLVMParseIRInContext2` and BitReader.h's `LLVMParseBitcodeInContext2` do not).
            unsafe { ffi::LLVMDisposeMemoryBuffer(self.0) }
        }
    }
}

struct MachineOwner(ffi::TargetMachineRef);
impl Drop for MachineOwner {
    fn drop(&mut self) {
        // SAFETY: created by `LLVMCreateTargetMachine`, disposed once.
        unsafe { ffi::LLVMDisposeTargetMachine(self.0) }
    }
}

struct OptionsOwner(ffi::PassBuilderOptionsRef);
impl Drop for OptionsOwner {
    fn drop(&mut self) {
        // SAFETY: created by `LLVMCreatePassBuilderOptions`, disposed once.
        unsafe { ffi::LLVMDisposePassBuilderOptions(self.0) }
    }
}

/// The remark stream: ended (the streamer deregistered from the context) before its buffer is
/// freed, on every path, so the context never writes into a freed stream.
struct SinkOwner {
    context: ffi::ContextRef,
    sink: ffi::RemarkSinkRef,
    ended: bool,
}

impl SinkOwner {
    /// Ends the stream; its YAML, borrowed from the sink.
    fn end(&mut self) -> &[u8] {
        let mut data: *const c_char = ptr::null();
        let mut len = 0usize;
        if !self.ended {
            // SAFETY: the sink was begun on this context and not ended; the context is alive
            // (its owner is declared before this one in `run`).
            unsafe { ffi::LLVMBlazeRemarksEnd(self.context, self.sink, &mut data, &mut len) };
            self.ended = true;
        }
        if data.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: the shim hands out the sink's own buffer, `len` bytes, live until the sink is
        // disposed, which needs `&mut self` (so not while this borrow lives).
        unsafe { core::slice::from_raw_parts(data.cast::<u8>(), len) }
    }
}

impl Drop for SinkOwner {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.end();
        }
        // SAFETY: begun by `LLVMBlazeRemarksBegin`, ended above, disposed once.
        unsafe { ffi::LLVMBlazeRemarksDispose(self.sink) }
    }
}

#[allow(clippy::too_many_arguments)]
fn run<'a>(
    ir: &[u8],
    offered: &[Linked<'_>],
    machine: &Machine,
    cpu: &str,
    pipeline: Pipeline,
    object: &mut AVec<'a, u8>,
    bitcode: Option<&mut AVec<'a, u8>>,
    remarks: &mut AVec<'a, Remark>,
    alloc: Alloc<'a>,
) -> Result<(), LlvmError<'a>> {
    if TARGETS.load(Ordering::Acquire) != READY {
        return Err(LlvmError::NotInitialized);
    }
    match cpu {
        "" => return Err(LlvmError::Cpu { what: "is empty" }),
        "native" => return Err(LlvmError::Cpu { what: "is `native`" }),
        _ if cpu.as_bytes().contains(&0) => return Err(LlvmError::Cpu { what: "holds a NUL byte" }),
        _ => {}
    }
    // An empty pipeline is refused just before it would run (below), so parsing, linking and
    // verifying are checked whatever the pipeline's state.
    let passes = pipeline.passes();
    let triple = triple_c(machine);
    let features = features_c(machine)?;
    let cpu_c = nul_terminated(cpu.as_bytes(), alloc);
    let passes_c = nul_terminated(passes.as_bytes(), alloc);
    // The IR parser reads up to a NUL after the text (LLLexer), so the buffer is the text and
    // one NUL, of the text's length, with `RequiresNullTerminator` set.
    let ir_c = nul_terminated(ir, alloc);

    // Declared before the context: the handler points at it while the context lives.
    // Every access, LLVM's handler's and ours, goes through this one pointer (no reference to it
    // is made while LLVM may write through it).
    let mut diagnostics = Diagnostics { severity: None, message: [0; MESSAGE], len: 0 };
    let diagnostics: *mut Diagnostics = &raw mut diagnostics;

    // SAFETY (the whole block): every handle is created here and owned by one owner above,
    // disposed once in the reverse of this order; every pointer handed to LLVM points at bytes
    // that outlive the call using it (`ir_c`, `cpu_c`, `passes_c`, `triple`, `features`, each
    // NUL-terminated); `diagnostics` outlives the context whose handler points at it and is read
    // only between LLVM calls.
    unsafe {
        let context = ContextOwner(ffi::LLVMContextCreate());
        ffi::LLVMContextSetDiagnosticHandler(
            context.0,
            Some(on_diagnostic),
            diagnostics.cast::<c_void>(),
        );

        let mut sink_ref: ffi::RemarkSinkRef = ptr::null_mut();
        let started =
            ffi::LLVMBlazeRemarksBegin(context.0, INLINER_PASSES.as_ptr().cast(), INLINER_PASSES.len(), &mut sink_ref);
        if !started.is_null() {
            return Err(LlvmError::RemarksStart { message: take_error(started, alloc) });
        }
        let mut sink = SinkOwner { context: context.0, sink: sink_ref, ended: false };

        // The target machine: named CPU, explicit features, PIC, the small code model, -O2's
        // code generator.
        let mut target: ffi::TargetRef = ptr::null_mut();
        let mut message: *mut c_char = ptr::null_mut();
        if ffi::LLVMGetTargetFromTriple(triple.as_ptr().cast(), &mut target, &mut message) != 0 {
            return Err(LlvmError::Target { triple: triple_str(triple), message: take_message(message, alloc) });
        }
        let tm = MachineOwner(ffi::LLVMCreateTargetMachine(
            target,
            triple.as_ptr().cast(),
            cpu_c.as_ptr().cast(),
            features.as_ptr().cast(),
            ffi::CODEGEN_LEVEL_DEFAULT,
            ffi::RELOC_PIC,
            ffi::CODE_MODEL_SMALL,
        ));

        // The caller's module.
        let buffer = BufferOwner(ffi::LLVMCreateMemoryBufferWithMemoryRange(
            ir_c.as_ptr().cast(),
            ir.len(),
            c"blaze-llvm".as_ptr(),
            1,
        ));
        let mut module = ModuleOwner(ptr::null_mut());
        let mut message: *mut c_char = ptr::null_mut();
        if ffi::LLVMParseIRInContext2(context.0, buffer.0, &mut module.0, &mut message) != 0 {
            return Err(LlvmError::Parse { message: take_message(message, alloc) });
        }
        drop(buffer);

        // The offered callees' bitcode, each its symbol's one definition made
        // `available_externally`, linked in.
        for linked in offered {
            link_one(context.0, module.0, linked, diagnostics, alloc)?;
        }

        let layout = ffi::LLVMCreateTargetDataLayout(tm.0);
        ffi::LLVMSetModuleDataLayout(module.0, layout);
        ffi::LLVMDisposeTargetData(layout);
        ffi::LLVMSetTarget(module.0, triple.as_ptr().cast());

        let mut message: *mut c_char = ptr::null_mut();
        if ffi::LLVMVerifyModule(module.0, ffi::RETURN_STATUS_ACTION, &mut message) != 0 {
            return Err(LlvmError::Verify { message: take_message(message, alloc) });
        }
        if !message.is_null() {
            ffi::LLVMDisposeMessage(message);
        }

        if passes.is_empty() {
            return Err(LlvmError::NoPipeline { pipeline: pipeline.name() });
        }
        let options = OptionsOwner(ffi::LLVMCreatePassBuilderOptions());
        let failed = ffi::LLVMRunPasses(module.0, passes_c.as_ptr().cast(), tm.0, options.0);
        if !failed.is_null() {
            return Err(LlvmError::Passes { pipeline: pipeline.name(), message: take_error(failed, alloc) });
        }
        drop(options);
        check(diagnostics, alloc)?;

        // The inliner ran inside the pipeline: its stream is complete now.
        let mut rows: AVec<'a, Remark> = Vec::new_in(alloc);
        remarks::read(sink.end(), &mut rows).map_err(|error| LlvmError::Remarks { error })?;
        remarks.extend_from_slice(&rows);
        remarks.sort_unstable();
        remarks.dedup();

        if let Some(bitcode) = bitcode {
            let written = BufferOwner(ffi::LLVMWriteBitcodeToMemoryBuffer(module.0));
            bitcode.extend_from_slice(buffer_bytes(written.0));
        }

        let mut emitted: ffi::MemoryBufferRef = ptr::null_mut();
        let mut message: *mut c_char = ptr::null_mut();
        if ffi::LLVMTargetMachineEmitToMemoryBuffer(tm.0, module.0, ffi::OBJECT_FILE, &mut message, &mut emitted) != 0 {
            return Err(LlvmError::Emit { message: take_message(message, alloc) });
        }
        let emitted = BufferOwner(emitted);
        check(diagnostics, alloc)?;
        object.extend_from_slice(buffer_bytes(emitted.0));
        drop(emitted);
        drop(module);
        drop(sink);
        drop(tm);
        drop(context);
    }
    Ok(())
}

/// Parses `linked`'s bitcode in `context`, makes its symbol's definition
/// `available_externally`, and links it into `dest`.
///
/// # Safety
///
/// `context` and `dest` are live, `dest` of `context`; `diagnostics` is the context's handler's.
unsafe fn link_one<'a>(
    context: ffi::ContextRef,
    dest: ffi::ModuleRef,
    linked: &Linked<'_>,
    diagnostics: *mut Diagnostics,
    alloc: Alloc<'a>,
) -> Result<(), LlvmError<'a>> {
    let symbol = linked.symbol;
    let name = ir_name(symbol);
    // SAFETY: the caller's contract for `context` and `dest`; `linked.bytes` outlives the
    // buffer, which borrows it and is disposed by its owner before this returns; the parsed
    // module is owned until `LLVMLinkModules2` takes it (Linker.h: it destroys the source).
    unsafe {
        let buffer = BufferOwner(ffi::LLVMCreateMemoryBufferWithMemoryRange(
            linked.bytes.as_ptr().cast(),
            linked.bytes.len(),
            c"blaze-llvm offered".as_ptr(),
            0,
        ));
        let mut src = ModuleOwner(ptr::null_mut());
        if ffi::LLVMParseBitcodeInContext2(context, buffer.0, &mut src.0) != 0 {
            return Err(LlvmError::Bitcode { symbol, what: "unreadable", message: diagnostic_text(diagnostics, alloc) });
        }
        drop(buffer);
        let mut found = false;
        let mut function = ffi::LLVMGetFirstFunction(src.0);
        while !function.is_null() {
            let defined = ffi::LLVMIsDeclaration(function) == 0;
            let linkage = ffi::LLVMGetLinkage(function);
            let local = linkage == ffi::LINKAGE_INTERNAL || linkage == ffi::LINKAGE_PRIVATE;
            if defined && value_name(function) == name.as_slice() {
                ffi::LLVMSetLinkage(function, ffi::LINKAGE_AVAILABLE_EXTERNALLY);
                found = true;
            } else if defined && !local {
                return Err(LlvmError::Bitcode {
                    symbol,
                    what: "defines a second exported function",
                    message: text_in(value_name(function), alloc),
                });
            }
            function = ffi::LLVMGetNextFunction(function);
        }
        if !found {
            return Err(LlvmError::Bitcode { symbol, what: "has no definition of its symbol", message: String::new_in(alloc) });
        }
        let mut global = ffi::LLVMGetFirstGlobal(src.0);
        while !global.is_null() {
            let linkage = ffi::LLVMGetLinkage(global);
            let local = linkage == ffi::LINKAGE_INTERNAL || linkage == ffi::LINKAGE_PRIVATE;
            if ffi::LLVMIsDeclaration(global) == 0 && !local {
                return Err(LlvmError::Bitcode {
                    symbol,
                    what: "defines an exported global",
                    message: text_in(value_name(global), alloc),
                });
            }
            global = ffi::LLVMGetNextGlobal(global);
        }
        let taken = src.0;
        src.0 = ptr::null_mut();
        if ffi::LLVMLinkModules2(dest, taken) != 0 {
            return Err(LlvmError::Link { symbol });
        }
    }
    Ok(())
}

/// The name of an LLVM value, borrowed from it.
///
/// # Safety
///
/// `value` is live and is not renamed while the slice is used.
unsafe fn value_name<'v>(value: ffi::ValueRef) -> &'v [u8] {
    let mut len = 0usize;
    // SAFETY: the caller's contract; Core.h: the name is the value's own storage.
    unsafe {
        let data = ffi::LLVMGetValueName2(value, &mut len);
        if data.is_null() || len == 0 {
            return &[];
        }
        core::slice::from_raw_parts(data.cast::<u8>(), len)
    }
}

/// A memory buffer's bytes, borrowed from it.
///
/// # Safety
///
/// `buffer` is live for `'b`.
unsafe fn buffer_bytes<'b>(buffer: ffi::MemoryBufferRef) -> &'b [u8] {
    // SAFETY: the caller's contract; Core.h gives the buffer's start and size.
    unsafe {
        let start = ffi::LLVMGetBufferStart(buffer);
        let size = ffi::LLVMGetBufferSize(buffer);
        if start.is_null() || size == 0 {
            return &[];
        }
        core::slice::from_raw_parts(start.cast::<u8>(), size)
    }
}

/// An error for the first error or warning the handler kept, if any. Called only between LLVM
/// calls, so nothing writes through the pointer while this reads.
fn check<'a>(diagnostics: *mut Diagnostics, alloc: Alloc<'a>) -> Result<(), LlvmError<'a>> {
    // SAFETY: `diagnostics` points at `run`'s live `Diagnostics`; LLVM is not running.
    let (severity, text) = unsafe { ((*diagnostics).severity, diagnostic_text(diagnostics, alloc)) };
    match severity {
        None => Ok(()),
        Some(severity) => Err(LlvmError::Diagnostic { severity, message: text }),
    }
}

fn diagnostic_text<'a>(diagnostics: *mut Diagnostics, alloc: Alloc<'a>) -> AString<'a> {
    // SAFETY: as `check`: a live `Diagnostics`, read between LLVM calls; the borrow ends here.
    unsafe {
        let kept = &*diagnostics;
        text_in(&kept.message[..kept.len], alloc)
    }
}

fn triple_str(triple: &'static [u8]) -> &'static str {
    match core::str::from_utf8(&triple[..triple.len() - 1]) {
        Ok(text) => text,
        Err(_) => "",
    }
}

fn nul_terminated<'a>(bytes: &[u8], alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut out = Vec::new_in(alloc);
    out.reserve(bytes.len() + 1);
    out.extend_from_slice(bytes);
    out.push(0);
    out
}
