//! The LLVM functions this crate calls, declared here (D1: own `extern "C"`): the C API of the
//! pinned LLVM 23.1.4, whose headers under `$LLVM_SYS_231_PREFIX/include/llvm-c` these
//! declarations were written against (IRReader.h's `LLVMParseIRInContext2`, which does NOT
//! consume its buffer; Remarks.h's parser; Core.h's diagnostic handler), plus the three remark
//! stream functions of blaze's wrappers (vendor/llvm-sys/wrappers/passes.cpp, wiring
//! wiring/llvm-sys/blaze-llvm-remarks-1.md), because the C API has no way to start a remark
//! stream (Remarks.h only parses one; LLVMRemarkStreamer.h's `setupLLVMOptimizationRemarks` is
//! C++).
//!
//! Every handle is an opaque pointer; who owns it is said at each function.

#![allow(non_camel_case_types, non_snake_case)]

use core::ffi::{c_char, c_int, c_uint, c_void};

/// An opaque LLVM object: never built or read in Rust, only pointed to.
macro_rules! opaque {
    ($($name:ident => $ref:ident;)*) => {$(
        #[repr(C)]
        pub struct $name {
            _private: [u8; 0],
        }
        pub type $ref = *mut $name;
    )*};
}

opaque! {
    OpaqueContext => ContextRef;
    OpaqueModule => ModuleRef;
    OpaqueMemoryBuffer => MemoryBufferRef;
    OpaqueValue => ValueRef;
    OpaqueTarget => TargetRef;
    OpaqueTargetMachine => TargetMachineRef;
    OpaqueTargetData => TargetDataRef;
    OpaquePassBuilderOptions => PassBuilderOptionsRef;
    OpaqueError => ErrorRef;
    OpaqueDiagnosticInfo => DiagnosticInfoRef;
    OpaqueRemarkParser => RemarkParserRef;
    OpaqueRemarkEntry => RemarkEntryRef;
    OpaqueRemarkArg => RemarkArgRef;
    OpaqueRemarkString => RemarkStringRef;
    BlazeRemarkSink => RemarkSinkRef;
}

pub type Bool = c_int;

pub type DiagnosticHandler = extern "C" fn(DiagnosticInfoRef, *mut c_void);

/// `LLVMDiagnosticSeverity`.
pub const DS_ERROR: c_int = 0;
pub const DS_WARNING: c_int = 1;
pub const DS_REMARK: c_int = 2;
pub const DS_NOTE: c_int = 3;

/// `LLVMVerifierFailureAction::LLVMReturnStatusAction`.
pub const RETURN_STATUS_ACTION: c_int = 2;

/// `LLVMCodeGenOptLevel::LLVMCodeGenLevelDefault` (-O2's code generator).
pub const CODEGEN_LEVEL_DEFAULT: c_int = 2;
/// `LLVMRelocMode::LLVMRelocPIC`.
pub const RELOC_PIC: c_int = 2;
/// `LLVMCodeModel::LLVMCodeModelSmall`.
pub const CODE_MODEL_SMALL: c_int = 3;
/// `LLVMCodeGenFileType::LLVMObjectFile`.
pub const OBJECT_FILE: c_int = 1;

/// `LLVMLinkage`.
pub const LINKAGE_EXTERNAL: c_int = 0;
pub const LINKAGE_AVAILABLE_EXTERNALLY: c_int = 1;
pub const LINKAGE_INTERNAL: c_int = 8;
pub const LINKAGE_PRIVATE: c_int = 9;

/// `LLVMRemarkType`.
pub const REMARK_UNKNOWN: c_uint = 0;
pub const REMARK_PASSED: c_uint = 1;
pub const REMARK_MISSED: c_uint = 2;
pub const REMARK_ANALYSIS: c_uint = 3;
pub const REMARK_ANALYSIS_FP_COMMUTE: c_uint = 4;
pub const REMARK_ANALYSIS_ALIASING: c_uint = 5;
pub const REMARK_FAILURE: c_uint = 6;

unsafe extern "C" {
    // Contexts and diagnostics (Core.h).
    pub fn LLVMContextCreate() -> ContextRef;
    pub fn LLVMContextDispose(context: ContextRef);
    pub fn LLVMContextSetDiagnosticHandler(
        context: ContextRef,
        handler: Option<DiagnosticHandler>,
        handler_context: *mut c_void,
    );
    /// Owned by the caller: freed with `LLVMDisposeMessage`.
    pub fn LLVMGetDiagInfoDescription(info: DiagnosticInfoRef) -> *mut c_char;
    pub fn LLVMGetDiagInfoSeverity(info: DiagnosticInfoRef) -> c_int;
    pub fn LLVMDisposeMessage(message: *mut c_char);

    // Memory buffers (Core.h).
    /// Borrows `data`, which must outlive the buffer.
    pub fn LLVMCreateMemoryBufferWithMemoryRange(
        data: *const c_char,
        length: usize,
        name: *const c_char,
        requires_null_terminator: Bool,
    ) -> MemoryBufferRef;
    pub fn LLVMGetBufferStart(buffer: MemoryBufferRef) -> *const c_char;
    pub fn LLVMGetBufferSize(buffer: MemoryBufferRef) -> usize;
    pub fn LLVMDisposeMemoryBuffer(buffer: MemoryBufferRef);

    // Reading modules (IRReader.h, BitReader.h).
    /// Does not consume `buffer`.
    pub fn LLVMParseIRInContext2(
        context: ContextRef,
        buffer: MemoryBufferRef,
        module: *mut ModuleRef,
        message: *mut *mut c_char,
    ) -> Bool;
    /// Does not consume `buffer`; reads it whole (not lazily).
    pub fn LLVMParseBitcodeInContext2(context: ContextRef, buffer: MemoryBufferRef, module: *mut ModuleRef) -> Bool;
    pub fn LLVMDisposeModule(module: ModuleRef);

    // Modules (Core.h, Analysis.h, Linker.h, BitWriter.h).
    pub fn LLVMVerifyModule(module: ModuleRef, action: c_int, message: *mut *mut c_char) -> Bool;
    /// Destroys `src`.
    pub fn LLVMLinkModules2(dest: ModuleRef, src: ModuleRef) -> Bool;
    pub fn LLVMSetTarget(module: ModuleRef, triple: *const c_char);
    pub fn LLVMSetModuleDataLayout(module: ModuleRef, layout: TargetDataRef);
    pub fn LLVMWriteBitcodeToMemoryBuffer(module: ModuleRef) -> MemoryBufferRef;
    pub fn LLVMGetFirstFunction(module: ModuleRef) -> ValueRef;
    pub fn LLVMGetNextFunction(function: ValueRef) -> ValueRef;
    pub fn LLVMGetFirstGlobal(module: ModuleRef) -> ValueRef;
    pub fn LLVMGetNextGlobal(global: ValueRef) -> ValueRef;
    /// Borrowed from the value: valid while it lives and is not renamed.
    pub fn LLVMGetValueName2(value: ValueRef, length: *mut usize) -> *const c_char;
    pub fn LLVMIsDeclaration(global: ValueRef) -> Bool;
    pub fn LLVMGetLinkage(global: ValueRef) -> c_int;
    pub fn LLVMSetLinkage(global: ValueRef, linkage: c_int);

    // Targets (TargetMachine.h, Target.h).
    pub fn LLVMGetTargetFromTriple(triple: *const c_char, target: *mut TargetRef, message: *mut *mut c_char) -> Bool;
    pub fn LLVMCreateTargetMachine(
        target: TargetRef,
        triple: *const c_char,
        cpu: *const c_char,
        features: *const c_char,
        level: c_int,
        reloc: c_int,
        code_model: c_int,
    ) -> TargetMachineRef;
    pub fn LLVMDisposeTargetMachine(machine: TargetMachineRef);
    pub fn LLVMCreateTargetDataLayout(machine: TargetMachineRef) -> TargetDataRef;
    pub fn LLVMDisposeTargetData(layout: TargetDataRef);
    pub fn LLVMTargetMachineEmitToMemoryBuffer(
        machine: TargetMachineRef,
        module: ModuleRef,
        file_type: c_int,
        message: *mut *mut c_char,
        out: *mut MemoryBufferRef,
    ) -> Bool;

    // The targets' registration (each target library's own entry points).
    pub fn LLVMInitializeAArch64TargetInfo();
    pub fn LLVMInitializeAArch64Target();
    pub fn LLVMInitializeAArch64TargetMC();
    pub fn LLVMInitializeAArch64AsmPrinter();
    pub fn LLVMInitializeX86TargetInfo();
    pub fn LLVMInitializeX86Target();
    pub fn LLVMInitializeX86TargetMC();
    pub fn LLVMInitializeX86AsmPrinter();

    // Passes (Transforms/PassBuilder.h, Error.h).
    pub fn LLVMCreatePassBuilderOptions() -> PassBuilderOptionsRef;
    pub fn LLVMDisposePassBuilderOptions(options: PassBuilderOptionsRef);
    /// Null on success; otherwise owned by the caller (consumed by `LLVMGetErrorMessage`).
    pub fn LLVMRunPasses(
        module: ModuleRef,
        passes: *const c_char,
        machine: TargetMachineRef,
        options: PassBuilderOptionsRef,
    ) -> ErrorRef;
    /// Consumes `error`; the message is freed with `LLVMDisposeErrorMessage`.
    pub fn LLVMGetErrorMessage(error: ErrorRef) -> *mut c_char;
    pub fn LLVMDisposeErrorMessage(message: *mut c_char);

    // The remark parser (Remarks.h).
    /// Borrows `buffer` for the parser's life.
    pub fn LLVMRemarkParserCreateYAML(buffer: *const c_void, size: u64) -> RemarkParserRef;
    /// Null at the end or on an error (`LLVMRemarkParserHasError` tells which).
    pub fn LLVMRemarkParserGetNext(parser: RemarkParserRef) -> RemarkEntryRef;
    pub fn LLVMRemarkParserHasError(parser: RemarkParserRef) -> Bool;
    /// Borrowed from the parser.
    pub fn LLVMRemarkParserGetErrorMessage(parser: RemarkParserRef) -> *const c_char;
    pub fn LLVMRemarkParserDispose(parser: RemarkParserRef);
    pub fn LLVMRemarkEntryDispose(entry: RemarkEntryRef);
    pub fn LLVMRemarkEntryGetType(entry: RemarkEntryRef) -> c_uint;
    pub fn LLVMRemarkEntryGetPassName(entry: RemarkEntryRef) -> RemarkStringRef;
    pub fn LLVMRemarkEntryGetRemarkName(entry: RemarkEntryRef) -> RemarkStringRef;
    pub fn LLVMRemarkEntryGetFunctionName(entry: RemarkEntryRef) -> RemarkStringRef;
    pub fn LLVMRemarkEntryGetFirstArg(entry: RemarkEntryRef) -> RemarkArgRef;
    pub fn LLVMRemarkEntryGetNextArg(arg: RemarkArgRef, entry: RemarkEntryRef) -> RemarkArgRef;
    pub fn LLVMRemarkArgGetKey(arg: RemarkArgRef) -> RemarkStringRef;
    pub fn LLVMRemarkArgGetValue(arg: RemarkArgRef) -> RemarkStringRef;
    /// Not NUL-terminated: `LLVMRemarkStringGetLen` bytes.
    pub fn LLVMRemarkStringGetData(string: RemarkStringRef) -> *const c_char;
    pub fn LLVMRemarkStringGetLen(string: RemarkStringRef) -> u32;

    // blaze's remark stream (vendor/llvm-sys/wrappers/passes.cpp; see the module).
    /// Starts streaming the remarks of the passes matching the regular expression
    /// `passes[..length]` as YAML into a buffer the sink owns. Null on success, the sink in
    /// `*sink`; an error if the context already streams remarks.
    pub fn LLVMBlazeRemarksBegin(
        context: ContextRef,
        passes: *const c_char,
        length: usize,
        sink: *mut RemarkSinkRef,
    ) -> ErrorRef;
    /// Finalizes the stream and deregisters it from the context; `*data`/`*length` are the
    /// YAML, borrowed from the sink until `LLVMBlazeRemarksDispose`.
    pub fn LLVMBlazeRemarksEnd(context: ContextRef, sink: RemarkSinkRef, data: *mut *const c_char, length: *mut usize);
    pub fn LLVMBlazeRemarksDispose(sink: RemarkSinkRef);
    /// LLVM's own `default<O{level}>` (level 1 to 3) for `machine`, as `opt
    /// -print-pipeline-passes` prints it; null for another level or a null machine. Owned by the
    /// caller: freed with `LLVMDisposeMessage`.
    pub fn LLVMBlazeDefaultPipeline(machine: TargetMachineRef, level: c_uint, slp_vectorization: c_int) -> *mut c_char;
}

/// The bytes of a remark string, borrowed from the entry it came from.
///
/// # Safety
///
/// `string` came from an entry that is still alive (Remarks.h: a string lives as long as its
/// entry); null gives the empty slice.
pub unsafe fn remark_bytes<'e>(string: RemarkStringRef) -> &'e [u8] {
    if string.is_null() {
        return &[];
    }
    // SAFETY: the caller's contract: the string is live; Remarks.h gives its data and length.
    unsafe {
        let data = LLVMRemarkStringGetData(string);
        let len = LLVMRemarkStringGetLen(string) as usize;
        if data.is_null() || len == 0 {
            return &[];
        }
        core::slice::from_raw_parts(data.cast::<u8>(), len)
    }
}

/// The bytes of a NUL-terminated C string LLVM handed out.
///
/// # Safety
///
/// `text` is null or a NUL-terminated string live for `'s`.
pub unsafe fn c_bytes<'s>(text: *const c_char) -> &'s [u8] {
    if text.is_null() {
        return &[];
    }
    // SAFETY: the caller's contract.
    unsafe { core::ffi::CStr::from_ptr(text) }.to_bytes()
}
