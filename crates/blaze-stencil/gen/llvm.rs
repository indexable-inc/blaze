//! LLVM through its C API: parse a module's text, compile it to an ELF object for one
//! variant, and disassemble stencils for the generator's checks. The one module of the
//! generator with `unsafe`; each call's invariant is said where it is made.

use std::{
    ffi::{CStr, CString, c_char},
    ptr,
    sync::Once,
};

use llvm_sys::{
    analysis::{LLVMVerifierFailureAction, LLVMVerifyModule},
    core::{
        LLVMContextCreate, LLVMContextDispose, LLVMCreateMemoryBufferWithMemoryRangeCopy,
        LLVMDisposeMemoryBuffer, LLVMDisposeMessage, LLVMDisposeModule, LLVMGetBufferSize,
        LLVMGetBufferStart, LLVMSetDataLayout, LLVMSetTarget,
    },
    disassembler::{
        LLVMCreateDisasmCPUFeatures, LLVMDisasmContextRef, LLVMDisasmDispose,
        LLVMDisasmInstruction,
    },
    ir_reader::LLVMParseIRInContext2,
    prelude::{LLVMMemoryBufferRef, LLVMModuleRef},
    target::{
        LLVMCopyStringRepOfTargetData, LLVMDisposeTargetData, LLVMInitializeAArch64AsmParser,
        LLVMInitializeAArch64AsmPrinter, LLVMInitializeAArch64Disassembler,
        LLVMInitializeAArch64Target, LLVMInitializeAArch64TargetInfo,
        LLVMInitializeAArch64TargetMC, LLVMInitializeX86AsmParser, LLVMInitializeX86AsmPrinter,
        LLVMInitializeX86Disassembler, LLVMInitializeX86Target, LLVMInitializeX86TargetInfo,
        LLVMInitializeX86TargetMC,
    },
    target_machine::{
        LLVMCodeGenFileType, LLVMCodeGenOptLevel, LLVMCodeModel, LLVMCreateTargetDataLayout,
        LLVMCreateTargetMachine, LLVMDisposeTargetMachine, LLVMGetTargetFromTriple,
        LLVMRelocMode, LLVMTargetMachineEmitToMemoryBuffer, LLVMTargetRef,
    },
};

use crate::format::Variant;

/// How LLVM is asked to compile one variant's stencils (stencil.md, "Why a build script").
#[derive(Clone, Copy, Debug)]
pub struct Machine {
    pub triple: &'static str,
    pub cpu: &'static str,
    pub features: &'static str,
    reloc: LLVMRelocMode,
    model: LLVMCodeModel,
}

impl Machine {
    /// x86-64: position-independent, so a place (the next stencil, a constant) is always
    /// RIP-relative, while the holes declared `!absolute_symbol` are absolute fields.
    /// AArch64: the tiny code model, so constants are `adr`/`ldr` literal (±1 MB) and never a
    /// page-relative `adrp`; `x18` reserved; atomics inline, never outlined into calls.
    pub const fn of(variant: Variant) -> Machine {
        match variant {
            Variant::X86_64 => Machine {
                triple: "x86_64-unknown-linux-gnu",
                cpu: "x86-64",
                features: "",
                reloc: LLVMRelocMode::LLVMRelocPIC,
                model: LLVMCodeModel::LLVMCodeModelSmall,
            },
            Variant::Aarch64 => Machine {
                triple: "aarch64-unknown-linux-gnu",
                cpu: "generic",
                features: "+neon,+reserve-x18,-outline-atomics,-lse",
                reloc: LLVMRelocMode::LLVMRelocStatic,
                model: LLVMCodeModel::LLVMCodeModelTiny,
            },
            Variant::Aarch64Lse => Machine {
                triple: "aarch64-unknown-linux-gnu",
                cpu: "generic",
                features: "+neon,+reserve-x18,-outline-atomics,+lse",
                reloc: LLVMRelocMode::LLVMRelocStatic,
                model: LLVMCodeModel::LLVMCodeModelTiny,
            },
        }
    }
}

/// Registers the two targets with LLVM, once per process.
pub fn init() {
    static INIT: Once = Once::new();
    // LLVM's target registration functions take no arguments and only fill LLVM's global
    // target registry; `Once` makes them run once, before any other call here.
    INIT.call_once(|| {
        let all: [unsafe extern "C" fn(); 12] = [
            LLVMInitializeX86TargetInfo,
            LLVMInitializeX86Target,
            LLVMInitializeX86TargetMC,
            LLVMInitializeX86AsmPrinter,
            LLVMInitializeX86AsmParser,
            LLVMInitializeX86Disassembler,
            LLVMInitializeAArch64TargetInfo,
            LLVMInitializeAArch64Target,
            LLVMInitializeAArch64TargetMC,
            LLVMInitializeAArch64AsmPrinter,
            LLVMInitializeAArch64AsmParser,
            LLVMInitializeAArch64Disassembler,
        ];
        for register in all {
            // SAFETY: as above: no arguments, once, before any other call into LLVM.
            unsafe { register() };
        }
    });
}

fn cstring(text: &str) -> CString {
    CString::new(text).expect("LLVM strings hold no NUL")
}

/// Takes an LLVM-allocated message, freeing it.
///
/// # Safety
/// `message` is null or a string LLVM allocated for its caller to free.
unsafe fn message(message: *mut c_char) -> String {
    if message.is_null() {
        return String::from("(no message)");
    }
    // SAFETY: the caller's contract: a NUL-terminated string from LLVM, live until freed below.
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned();
    // SAFETY: the caller's contract: LLVM allocated it for the caller to free, once, here.
    unsafe { LLVMDisposeMessage(message) };
    text
}

/// Compiles one module of LLVM IR text for `machine` at `-O2`; the ELF object's bytes.
pub fn compile(machine: Machine, name: &str, ir: &str) -> Result<Vec<u8>, String> {
    init();
    let triple = cstring(machine.triple);
    let cpu = cstring(machine.cpu);
    let features = cstring(machine.features);
    let buffer_name = cstring(name);
    // Every pointer handed to LLVM is a live `CString` or the text's own bytes with its length;
    // every object LLVM returns is disposed exactly once below, on every path, and none is used
    // after its disposal. The context is used by this thread alone.
    // SAFETY: takes nothing; the context is disposed on every path below.
    let context = unsafe { LLVMContextCreate() };
    // SAFETY: the text's bytes with their length and a live `CString`; LLVM copies them.
    let buffer = unsafe {
        LLVMCreateMemoryBufferWithMemoryRangeCopy(
            ir.as_ptr().cast(),
            ir.len(),
            buffer_name.as_ptr(),
        )
    };
    let mut module: LLVMModuleRef = ptr::null_mut();
    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: a live context and buffer, and two places for LLVM to write its answers.
    let parsed = unsafe { LLVMParseIRInContext2(context, buffer, &mut module, &mut error) };
    // SAFETY: the buffer made above, parsed and used no more.
    unsafe { LLVMDisposeMemoryBuffer(buffer) };
    if parsed != 0 {
        // SAFETY: the context made above, with no module of it left.
        unsafe { LLVMContextDispose(context) };
        // SAFETY: what the parse left in `error`, null or LLVM's to free.
        let why = unsafe { message(error) };
        return Err(format!("LLVM refused the stencils' IR of {name}: {why}"));
    }
    // SAFETY: the module just parsed is live, and the strings are.
    let result = unsafe { emit(machine, &triple, &cpu, &features, module, name) };
    // SAFETY: the module parsed above, used no more.
    unsafe { LLVMDisposeModule(module) };
    // SAFETY: the context made above, its module disposed.
    unsafe { LLVMContextDispose(context) };
    result
}

/// # Safety
/// `module` is a live module; the strings are live.
unsafe fn emit(
    machine: Machine,
    triple: &CString,
    cpu: &CString,
    features: &CString,
    module: LLVMModuleRef,
    name: &str,
) -> Result<Vec<u8>, String> {
    // As `compile` says; the target machine and data layout are disposed here.
    let mut target: LLVMTargetRef = ptr::null_mut();
    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: a live `CString` and two places for LLVM to write its answers.
    if unsafe { LLVMGetTargetFromTriple(triple.as_ptr(), &mut target, &mut error) } != 0 {
        // SAFETY: what the lookup left in `error`, null or LLVM's to free.
        return Err(format!("LLVM has no target {}: {}", machine.triple, unsafe { message(error) }));
    }
    // SAFETY: the target LLVM just gave and live `CString`s; the machine is disposed below.
    let tm = unsafe {
        LLVMCreateTargetMachine(
            target,
            triple.as_ptr(),
            cpu.as_ptr(),
            features.as_ptr(),
            LLVMCodeGenOptLevel::LLVMCodeGenLevelDefault,
            machine.reloc,
            machine.model,
        )
    };
    if tm.is_null() {
        return Err(format!("LLVM made no target machine for {}", machine.triple));
    }
    // SAFETY: the caller's live module and a live `CString`, which LLVM copies.
    unsafe { LLVMSetTarget(module, triple.as_ptr()) };
    // SAFETY: the live target machine; the layout is disposed below.
    let layout = unsafe { LLVMCreateTargetDataLayout(tm) };
    // SAFETY: the live layout; the text is LLVM's, freed below.
    let layout_text = unsafe { LLVMCopyStringRepOfTargetData(layout) };
    // SAFETY: the live module and the layout's text, which LLVM copies.
    unsafe { LLVMSetDataLayout(module, layout_text) };
    // SAFETY: the text LLVM made above, used no more.
    unsafe { LLVMDisposeMessage(layout_text) };
    // SAFETY: the layout made above, used no more.
    unsafe { LLVMDisposeTargetData(layout) };
    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: the live module and a place for LLVM to write its message.
    let refused = unsafe {
        LLVMVerifyModule(module, LLVMVerifierFailureAction::LLVMReturnStatusAction, &mut error)
    };
    if refused != 0 {
        // SAFETY: the machine made above, used no more.
        unsafe { LLVMDisposeTargetMachine(tm) };
        // SAFETY: what the check left in `error`, null or LLVM's to free.
        let why = unsafe { message(error) };
        return Err(format!("the stencils' IR of {name} is not valid: {why}"));
    }
    if !error.is_null() {
        // SAFETY: a message the check made, LLVM's to free, once.
        unsafe { LLVMDisposeMessage(error) };
    }
    let mut out: LLVMMemoryBufferRef = ptr::null_mut();
    let mut error: *mut c_char = ptr::null_mut();
    // SAFETY: the live machine and module, and two places for LLVM to write its answers.
    let failed = unsafe {
        LLVMTargetMachineEmitToMemoryBuffer(
            tm,
            module,
            LLVMCodeGenFileType::LLVMObjectFile,
            &mut error,
            &mut out,
        )
    };
    // SAFETY: the machine made above, used no more.
    unsafe { LLVMDisposeTargetMachine(tm) };
    if failed != 0 {
        // SAFETY: what the emit left in `error`, null or LLVM's to free.
        let why = unsafe { message(error) };
        return Err(format!("LLVM did not compile the stencils of {name}: {why}"));
    }
    // SAFETY: the live buffer LLVM just filled.
    let start = unsafe { LLVMGetBufferStart(out) }.cast::<u8>();
    // SAFETY: the live buffer LLVM just filled.
    let len = unsafe { LLVMGetBufferSize(out) };
    // SAFETY: `start..start + len` is the buffer's bytes, live until it is disposed below.
    let bytes = unsafe { std::slice::from_raw_parts(start, len) }.to_vec();
    // SAFETY: the buffer the emit made, its bytes copied out.
    unsafe { LLVMDisposeMemoryBuffer(out) };
    Ok(bytes)
}

/// LLVM's disassembler for one variant, for the checks: each instruction's length and text.
pub struct Disasm {
    context: LLVMDisasmContextRef,
}

impl Disasm {
    pub fn new(machine: Machine) -> Result<Disasm, String> {
        init();
        let triple = cstring(machine.triple);
        let cpu = cstring(machine.cpu);
        let features = cstring(machine.features);
        // SAFETY: live strings; no callbacks and no user data, which LLVM accepts as null.
        let context = unsafe {
            LLVMCreateDisasmCPUFeatures(
                triple.as_ptr(),
                cpu.as_ptr(),
                features.as_ptr(),
                ptr::null_mut(),
                0,
                None,
                None,
            )
        };
        if context.is_null() {
            return Err(format!("LLVM has no disassembler for {}", machine.triple));
        }
        Ok(Disasm { context })
    }

    /// The instruction at the start of `code` (at `pc`): its length and text, or `None` when
    /// the bytes are no instruction.
    pub fn inst(&self, code: &[u8], pc: u64) -> Option<(usize, String)> {
        let mut text = [0 as c_char; 256];
        // SAFETY: LLVM reads at most `code.len()` bytes (it takes a `*mut` but does not write
        // through it) and writes at most `text.len()` bytes, NUL-terminated, into `text`.
        let len = unsafe {
            LLVMDisasmInstruction(
                self.context,
                code.as_ptr().cast_mut(),
                code.len() as u64,
                pc,
                text.as_mut_ptr(),
                text.len(),
            )
        };
        if len == 0 {
            return None;
        }
        // SAFETY: LLVM NUL-terminated what it wrote, within `text`.
        let text = unsafe { CStr::from_ptr(text.as_ptr()) }.to_string_lossy().trim().to_owned();
        Some((len, text))
    }
}

impl Drop for Disasm {
    fn drop(&mut self) {
        // SAFETY: the context was made by `new` and is disposed once, here.
        unsafe { LLVMDisasmDispose(self.context) }
    }
}
