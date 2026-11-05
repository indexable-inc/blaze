//! Bindings to LLVM's C API.
//!
//! Refer to the [LLVM documentation](http://llvm.org/docs/) for more
//! information.
//!
//! # The contract (blaze)
//!
//! This crate is declarations only: no function body, no logic, no `unsafe` block
//! (`forbid(unsafe_code)` below holds it to that). Every call into it is `unsafe`, and in blaze
//! every one is made behind a safe wrapper in `crates/hir-ty-codegen/llvm.rs` and `llvm/` (or
//! in inkwell, which hir-ty-codegen calls through), whose `// SAFETY:` comments say which of
//! the rules below each call keeps.
//!
//! ## Who owns each `LLVM*Ref`
//!
//! - `LLVMContextRef`: whoever called `LLVMContextCreate`, until `LLVMContextDispose`. The
//!   context owns every type, constant, metadata node and attribute made in it
//!   (`LLVMTypeRef`, constant `LLVMValueRef`s, `LLVMMetadataRef`, `LLVMAttributeRef`): none of
//!   those is freed on its own, and all of them dangle once the context is disposed of.
//!   `LLVMGetGlobalContext` is one context for the whole process; blaze never uses it.
//! - `LLVMModuleRef`: whoever created or parsed it, until `LLVMDisposeModule`, which must come
//!   before the disposal of its context. The module owns its functions and globals.
//!   `LLVMLinkModules2(Dest, Src)` destroys `Src` whether or not it fails.
//! - Functions, globals, basic blocks and instructions (`LLVMValueRef`, `LLVMBasicBlockRef`)
//!   are owned by their parent (module, function, block). `LLVMDeleteFunction`,
//!   `LLVMDeleteGlobal`, `LLVMDeleteBasicBlock` and `LLVMInstructionEraseFromParent` free them;
//!   the handle, and any handle to a use of it, dangles after. Free one only when nothing uses
//!   it (`LLVMReplaceAllUsesWith` first), and take the next instruction of a walk before
//!   erasing the current one. `LLVMInstructionRemoveFromParent` unlinks without freeing.
//! - `LLVMUseRef` is a view of one entry of a value's use list: valid until that list changes
//!   (a use added, replaced or erased).
//! - `LLVMBuilderRef` (`LLVMCreateBuilderInContext`) and `LLVMDIBuilderRef`
//!   (`LLVMCreateDIBuilder`, finalized with `LLVMDIBuilderFinalize`): the caller's, until
//!   `LLVMDisposeBuilder` / `LLVMDisposeDIBuilder`, before their context or module is disposed
//!   of. A builder only inserts into blocks of its own context.
//! - `LLVMMemoryBufferRef`: the caller's, until `LLVMDisposeMemoryBuffer`.
//!   `LLVMCreateMemoryBufferWithMemoryRange` does not copy: the bytes must outlive the buffer
//!   (`..RangeCopy` copies). `LLVMParseIRInContext2` and `LLVMParseBitcodeInContext2` leave the
//!   buffer the caller's; the deprecated `LLVMParseIRInContext` takes it.
//! - `LLVMTargetRef` is a registry entry that lives for the process and is never freed.
//!   `LLVMTargetMachineRef` (`LLVMCreateTargetMachine`) is the caller's until
//!   `LLVMDisposeTargetMachine`; `LLVMTargetDataRef` from `LLVMCreateTargetDataLayout` until
//!   `LLVMDisposeTargetData`.
//! - `LLVMPassBuilderOptionsRef`: the caller's, until `LLVMDisposePassBuilderOptions`.
//! - blaze's `LLVMBlazePassesRef` (`transforms::pass_builder`, `wrappers/passes.cpp`): the
//!   caller's, until `LLVMBlazeDisposePasses`, which must come before the disposal of the
//!   target machine it was made for, and of the profile bytes it was given. It is bound to the
//!   first context it runs a module of and refuses a module of another.
//! - `LLVMErrorRef`: null is success. A non-null error is owned by whoever got it and must be
//!   consumed exactly once, by `LLVMConsumeError` or by `LLVMGetErrorMessage`, whose message is
//!   then freed with `LLVMDisposeErrorMessage` (not `LLVMDisposeMessage`).
//! - `LLVMDiagnosticInfoRef`, given to a diagnostic handler, lives for that call only.
//!
//! ## Strings
//!
//! - A `*const c_char` argument with no length beside it is read up to its NUL: it must be
//!   NUL-terminated (`c"..."` or a `CString`) and live for the call. One with a length
//!   (`LLVMSetValueName2`, `LLVMBlazeRunPasses`' `Pipeline`, `LLVMBlazeHasFeature`'s `Name`) is
//!   read for that many bytes and needs no NUL.
//! - A `*mut c_char` returned, or written through a `*mut *mut c_char` out-parameter (the
//!   `ErrorMessage`/`OutMessage` of `LLVMVerifyModule`, `LLVMTargetMachineEmitToMemoryBuffer`,
//!   `LLVMGetTargetFromTriple`, `LLVMParseIRInContext2`, ...), is a new allocation the caller
//!   owns and frees once with `LLVMDisposeMessage`: `LLVMPrintModuleToString`,
//!   `LLVMPrintValueToString`, `LLVMPrintTypeToString`, `LLVMGetDefaultTargetTriple`,
//!   `LLVMNormalizeTargetTriple`, `LLVMGetHostCPUName`, `LLVMGetHostCPUFeatures`,
//!   `LLVMGetTargetMachineTriple`, `LLVMGetTargetMachineCPU`,
//!   `LLVMGetTargetMachineFeatureString`, `LLVMCopyStringRepOfTargetData`,
//!   `LLVMGetDiagInfoDescription`, `LLVMCreateMessage`. Start an out-parameter null and free
//!   it whenever it is non-null after the call, failed or not: `LLVMVerifyModule` writes one
//!   (empty) on success too.
//! - A `*const c_char` returned (`LLVMGetValueName2`, `LLVMGetMDString`, `LLVMGetTargetName`,
//!   `LLVMGetSectionName`, `LLVMGetBufferStart`) is borrowed from its owner: never freed, and
//!   valid only while the owner lives and is unchanged.
//!
//! ## Threads
//!
//! - A context and everything in it (its modules, builders, values, types and metadata) is used
//!   by one thread at a time. Separate contexts can be used on separate threads at once: blaze
//!   keeps one per thread (`hir-ty-codegen` `llvm::Machine`, thread-local), with that thread's
//!   target machines and passes, so no handle crosses threads.
//! - Process-wide state is set up once, before threads use it: `LLVMParseCommandLineOptions`
//!   (blaze calls it under a `Once`, before any target machine is made) and the target
//!   initializers (`LLVMInitializeX86Target` and the rest), which write LLVM's target
//!   registry: blaze calls them through inkwell's `Target::initialize_*`, which holds a
//!   process-wide lock around each.
//! - A diagnostic handler (`LLVMContextSetDiagnosticHandler`) runs on the thread doing the work
//!   in that context. A callback must not unwind into LLVM: an `extern "C"` fn that panics
//!   aborts.
//!
//! ## `LLVMBool`
//!
//! `0` is false and anything else true. For the functions that report failure through it
//! (`LLVMVerifyModule`, `LLVMTargetMachineEmitToMemoryBuffer`, the parsers, `LLVMLinkModules2`)
//! true means failed.

#![forbid(unsafe_code)]
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]

extern crate libc;

use self::prelude::*;

#[derive(Debug)]
pub enum LLVMMemoryBuffer {}

#[derive(Debug)]
pub enum LLVMContext {}

#[derive(Debug)]
pub enum LLVMModule {}

#[derive(Debug)]
pub enum LLVMType {}

#[derive(Debug)]
pub enum LLVMValue {}

#[derive(Debug)]
pub enum LLVMBasicBlock {}

#[derive(Debug)]
pub enum LLVMOpaqueMetadata {}

#[derive(Debug)]
pub enum LLVMOpaqueNamedMDNode {}

#[derive(Debug)]
pub enum LLVMOpaqueValueMetadataEntry {}

#[derive(Debug)]
pub enum LLVMBuilder {}

#[derive(Debug)]
pub enum LLVMOpaqueDIBuilder {}

#[derive(Debug)]
pub enum LLVMModuleProvider {}

#[derive(Debug)]
pub enum LLVMPassManager {}

#[derive(Debug)]
pub enum LLVMUse {}

#[derive(Debug)]
pub enum LLVMOpaqueOperandBundle {}

#[derive(Debug)]
pub enum LLVMDiagnosticInfo {}

#[derive(Debug)]
pub enum LLVMComdat {}

#[derive(Debug)]
pub enum LLVMOpaqueModuleFlagEntry {}

#[derive(Debug)]
pub enum LLVMOpaqueJITEventListener {}

#[derive(Debug)]
pub enum LLVMOpaqueAttributeRef {}

#[derive(Debug)]
pub enum LLVMOpaqueDbgRecord {}

/// Core types used throughout LLVM.
///
/// In most cases you will want to `use llvm::prelude::*`.
pub mod prelude {
    pub type LLVMBool = ::libc::c_int;
    pub type LLVMMemoryBufferRef = *mut super::LLVMMemoryBuffer;
    pub type LLVMContextRef = *mut super::LLVMContext;
    pub type LLVMModuleRef = *mut super::LLVMModule;
    pub type LLVMTypeRef = *mut super::LLVMType;
    pub type LLVMValueRef = *mut super::LLVMValue;
    pub type LLVMBasicBlockRef = *mut super::LLVMBasicBlock;
    pub type LLVMMetadataRef = *mut super::LLVMOpaqueMetadata;
    pub type LLVMNamedMDNodeRef = *mut super::LLVMOpaqueNamedMDNode;
    pub type LLVMValueMetadataEntry = *mut super::LLVMOpaqueValueMetadataEntry;
    pub type LLVMBuilderRef = *mut super::LLVMBuilder;
    pub type LLVMDIBuilderRef = *mut super::LLVMOpaqueDIBuilder;
    pub type LLVMModuleProviderRef = *mut super::LLVMModuleProvider;
    pub type LLVMPassManagerRef = *mut super::LLVMPassManager;
    pub type LLVMUseRef = *mut super::LLVMUse;
    pub type LLVMOperandBundleRef = *mut super::LLVMOpaqueOperandBundle;
    pub type LLVMDiagnosticInfoRef = *mut super::LLVMDiagnosticInfo;
    pub type LLVMComdatRef = *mut super::LLVMComdat;
    pub type LLVMModuleFlagEntry = *mut super::LLVMOpaqueModuleFlagEntry;
    pub type LLVMJITEventListenerRef = *mut super::LLVMOpaqueJITEventListener;
    pub type LLVMAttributeRef = *mut super::LLVMOpaqueAttributeRef;
    pub type LLVMDbgRecordRef = *mut super::LLVMOpaqueDbgRecord;
}

pub mod analysis;
pub mod bit_reader;
pub mod bit_writer;
pub mod blake3;
pub mod comdat;
pub mod core;
pub mod debuginfo;
pub mod disassembler;
pub mod error;
pub mod error_handling;
pub mod execution_engine;
pub mod ir_reader;
pub mod linker;
pub mod lto;
pub mod object;
pub mod orc2;
pub mod remarks;
pub mod support;
pub mod target;
pub mod target_machine;

pub mod transforms {
    pub mod pass_builder;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMOpcode {
    LLVMRet = 1,
    // LLVMBr = 2 was removed in LLVM 23
    LLVMUncondBr = 70,
    LLVMCondBr = 71,
    LLVMSwitch = 3,
    LLVMIndirectBr = 4,
    LLVMInvoke = 5,
    LLVMUnreachable = 7,
    LLVMCallBr = 67,
    LLVMFNeg = 66,
    LLVMAdd = 8,
    LLVMFAdd = 9,
    LLVMSub = 10,
    LLVMFSub = 11,
    LLVMMul = 12,
    LLVMFMul = 13,
    LLVMUDiv = 14,
    LLVMSDiv = 15,
    LLVMFDiv = 16,
    LLVMURem = 17,
    LLVMSRem = 18,
    LLVMFRem = 19,
    LLVMShl = 20,
    LLVMLShr = 21,
    LLVMAShr = 22,
    LLVMAnd = 23,
    LLVMOr = 24,
    LLVMXor = 25,
    LLVMAlloca = 26,
    LLVMLoad = 27,
    LLVMStore = 28,
    LLVMGetElementPtr = 29,
    LLVMTrunc = 30,
    LLVMZExt = 31,
    LLVMSExt = 32,
    LLVMFPToUI = 33,
    LLVMFPToSI = 34,
    LLVMUIToFP = 35,
    LLVMSIToFP = 36,
    LLVMFPTrunc = 37,
    LLVMFPExt = 38,
    LLVMPtrToInt = 39,
    LLVMPtrToAddr = 69,
    LLVMIntToPtr = 40,
    LLVMBitCast = 41,
    LLVMAddrSpaceCast = 60,
    LLVMICmp = 42,
    LLVMFCmp = 43,
    LLVMPHI = 44,
    LLVMCall = 45,
    LLVMSelect = 46,
    LLVMUserOp1 = 47,
    LLVMUserOp2 = 48,
    LLVMVAArg = 49,
    LLVMExtractElement = 50,
    LLVMInsertElement = 51,
    LLVMShuffleVector = 52,
    LLVMExtractValue = 53,
    LLVMInsertValue = 54,
    LLVMFreeze = 68,
    LLVMFence = 55,
    LLVMAtomicCmpXchg = 56,
    LLVMAtomicRMW = 57,
    LLVMResume = 58,
    LLVMLandingPad = 59,
    LLVMCleanupRet = 61,
    LLVMCatchRet = 62,
    LLVMCatchPad = 63,
    LLVMCleanupPad = 64,
    LLVMCatchSwitch = 65,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMTypeKind {
    LLVMVoidTypeKind = 0,
    LLVMHalfTypeKind = 1,
    LLVMFloatTypeKind = 2,
    LLVMDoubleTypeKind = 3,
    LLVMX86_FP80TypeKind = 4,
    LLVMFP128TypeKind = 5,
    LLVMPPC_FP128TypeKind = 6,
    LLVMLabelTypeKind = 7,
    LLVMIntegerTypeKind = 8,
    LLVMFunctionTypeKind = 9,
    LLVMStructTypeKind = 10,
    LLVMArrayTypeKind = 11,
    LLVMPointerTypeKind = 12,
    LLVMVectorTypeKind = 13,
    LLVMMetadataTypeKind = 14,
    // 15 previously used by LLVMX86_MMXTypeKind
    LLVMTokenTypeKind = 16,
    LLVMScalableVectorTypeKind = 17,
    LLVMBFloatTypeKind = 18,
    LLVMX86_AMXTypeKind = 19,
    LLVMTargetExtTypeKind = 20,
    /// Arbitrary bit width bytes
    LLVMByteTypeKind = 21,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMLinkage {
    LLVMExternalLinkage = 0,
    LLVMAvailableExternallyLinkage = 1,
    LLVMLinkOnceAnyLinkage = 2,
    LLVMLinkOnceODRLinkage = 3,
    LLVMLinkOnceODRAutoHideLinkage = 4,
    LLVMWeakAnyLinkage = 5,
    LLVMWeakODRLinkage = 6,
    LLVMAppendingLinkage = 7,
    LLVMInternalLinkage = 8,
    LLVMPrivateLinkage = 9,
    LLVMDLLImportLinkage = 10,
    LLVMDLLExportLinkage = 11,
    LLVMExternalWeakLinkage = 12,
    LLVMGhostLinkage = 13,
    LLVMCommonLinkage = 14,
    LLVMLinkerPrivateLinkage = 15,
    LLVMLinkerPrivateWeakLinkage = 16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMVisibility {
    LLVMDefaultVisibility = 0,
    LLVMHiddenVisibility = 1,
    LLVMProtectedVisibility = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMUnnamedAddr {
    /// Address of the GV is significant.
    LLVMNoUnnamedAddr,
    /// Address of the GV is locally insignificant.
    LLVMLocalUnnamedAddr,
    /// Address of the GV is globally insignificant.
    LLVMGlobalUnnamedAddr,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMDLLStorageClass {
    LLVMDefaultStorageClass = 0,
    LLVMDLLImportStorageClass = 1,
    LLVMDLLExportStorageClass = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMCallConv {
    LLVMCCallConv = 0,
    LLVMFastCallConv = 8,
    LLVMColdCallConv = 9,
    LLVMGHCCallConv = 10,
    LLVMHiPECallConv = 11,
    LLVMAnyRegCallConv = 13,
    LLVMPreserveMostCallConv = 14,
    LLVMPreserveAllCallConv = 15,
    LLVMSwiftCallConv = 16,
    LLVMCXXFASTTLSCallConv = 17,
    LLVMX86StdcallCallConv = 64,
    LLVMX86FastcallCallConv = 65,
    LLVMARMAPCSCallConv = 66,
    LLVMARMAAPCSCallConv = 67,
    LLVMARMAAPCSVFPCallConv = 68,
    LLVMMSP430INTRCallConv = 69,
    LLVMX86ThisCallCallConv = 70,
    LLVMPTXKernelCallConv = 71,
    LLVMPTXDeviceCallConv = 72,
    LLVMSPIRFUNCCallConv = 75,
    LLVMSPIRKERNELCallConv = 76,
    LLVMIntelOCLBICallConv = 77,
    LLVMX8664SysVCallConv = 78,
    LLVMWin64CallConv = 79,
    LLVMX86VectorCallCallConv = 80,
    LLVMHHVMCallConv = 81,
    LLVMHHVMCCallConv = 82,
    LLVMX86INTRCallConv = 83,
    LLVMAVRINTRCallConv = 84,
    LLVMAVRSIGNALCallConv = 85,
    LLVMAVRBUILTINCallConv = 86,
    LLVMAMDGPUVSCallConv = 87,
    LLVMAMDGPUGSCallConv = 88,
    LLVMAMDGPUPSCallConv = 89,
    LLVMAMDGPUCSCallConv = 90,
    LLVMAMDGPUKERNELCallConv = 91,
    LLVMX86RegCallCallConv = 92,
    LLVMAMDGPUHSCallConv = 93,
    LLVMMSP430BUILTINCallConv = 94,
    LLVMAMDGPULSCallConv = 95,
    LLVMAMDGPUESCallConv = 96,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMValueKind {
    LLVMArgumentValueKind,
    LLVMBasicBlockValueKind,
    LLVMMemoryUseValueKind,
    LLVMMemoryDefValueKind,
    LLVMMemoryPhiValueKind,

    LLVMFunctionValueKind,
    LLVMGlobalAliasValueKind,
    LLVMGlobalIFuncValueKind,
    LLVMGlobalVariableValueKind,
    LLVMBlockAddressValueKind,
    LLVMConstantExprValueKind,
    LLVMConstantArrayValueKind,
    LLVMConstantStructValueKind,
    LLVMConstantVectorValueKind,
    LLVMUndefValueValueKind,
    LLVMConstantAggregateZeroValueKind,
    LLVMConstantDataArrayValueKind,
    LLVMConstantDataVectorValueKind,
    LLVMConstantIntValueKind,
    LLVMConstantByteValueKind,
    LLVMConstantFPValueKind,
    LLVMConstantPointerNullValueKind,
    LLVMConstantTokenNoneValueKind,

    LLVMMetadataAsValueValueKind,
    LLVMInlineAsmValueKind,

    LLVMInstructionValueKind,
    LLVMPoisonValueKind,
    LLVMConstantTargetNoneValueKind,
    LLVMConstantPtrAuthValueKind,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMIntPredicate {
    LLVMIntEQ = 32,
    LLVMIntNE = 33,
    LLVMIntUGT = 34,
    LLVMIntUGE = 35,
    LLVMIntULT = 36,
    LLVMIntULE = 37,
    LLVMIntSGT = 38,
    LLVMIntSGE = 39,
    LLVMIntSLT = 40,
    LLVMIntSLE = 41,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMRealPredicate {
    LLVMRealPredicateFalse = 0,
    LLVMRealOEQ = 1,
    LLVMRealOGT = 2,
    LLVMRealOGE = 3,
    LLVMRealOLT = 4,
    LLVMRealOLE = 5,
    LLVMRealONE = 6,
    LLVMRealORD = 7,
    LLVMRealUNO = 8,
    LLVMRealUEQ = 9,
    LLVMRealUGT = 10,
    LLVMRealUGE = 11,
    LLVMRealULT = 12,
    LLVMRealULE = 13,
    LLVMRealUNE = 14,
    LLVMRealPredicateTrue = 15,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMThreadLocalMode {
    LLVMNotThreadLocal = 0,
    LLVMGeneralDynamicTLSModel = 1,
    LLVMLocalDynamicTLSModel = 2,
    LLVMInitialExecTLSModel = 3,
    LLVMLocalExecTLSModel = 4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMAtomicOrdering {
    LLVMAtomicOrderingNotAtomic = 0,
    LLVMAtomicOrderingUnordered = 1,
    LLVMAtomicOrderingMonotonic = 2,
    LLVMAtomicOrderingAcquire = 4,
    LLVMAtomicOrderingRelease = 5,
    LLVMAtomicOrderingAcquireRelease = 6,
    LLVMAtomicOrderingSequentiallyConsistent = 7,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMAtomicRMWBinOp {
    LLVMAtomicRMWBinOpXchg = 0,
    LLVMAtomicRMWBinOpAdd = 1,
    LLVMAtomicRMWBinOpSub = 2,
    LLVMAtomicRMWBinOpAnd = 3,
    LLVMAtomicRMWBinOpNand = 4,
    LLVMAtomicRMWBinOpOr = 5,
    LLVMAtomicRMWBinOpXor = 6,
    LLVMAtomicRMWBinOpMax = 7,
    LLVMAtomicRMWBinOpMin = 8,
    LLVMAtomicRMWBinOpUMax = 9,
    LLVMAtomicRMWBinOpUMin = 10,
    LLVMAtomicRMWBinOpFAdd = 11,
    LLVMAtomicRMWBinOpFSub = 12,
    LLVMAtomicRMWBinOpFMax = 13,
    LLVMAtomicRMWBinOpFMin = 14,
    LLVMAtomicRMWBinOpUIncWrap = 15,
    LLVMAtomicRMWBinOpUDecWrap = 16,
    /// Subtracts the value only if no unsigned overflow.
    LLVMAtomicRMWBinOpUSubCond,
    /// Subtracts the value, clamping to zero.
    LLVMAtomicRMWBinOpUSubSat,
    /// Sets the value if it's greater than the original using an floating
    /// point comparison and return the old one.
    LLVMAtomicRMWBinOpFMaximum,
    /// Sets the value if it's smaller than the original using an floating
    /// point comparison and return the old one.
    LLVMAtomicRMWBinOpFMinimum,
    /// Sets the value if it's greater than the original using a floating
    /// point comparison and return the old one.
    LLVMAtomicRMWBinOpFMaximumNum,
    /// Sets the value if it's smaller than the original using a floating
    /// point comparison and return the old one.
    LLVMAtomicRMWBinOpFMinimumNum,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMDiagnosticSeverity {
    LLVMDSError = 0,
    LLVMDSWarning = 1,
    LLVMDSRemark = 2,
    LLVMDSNote = 3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMInlineAsmDialect {
    LLVMInlineAsmDialectATT,
    LLVMInlineAsmDialectIntel,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMModuleFlagBehavior {
    /// Emits an error if two values disagree, otherwise the resulting value is that of the operands.
    LLVMModuleFlagBehaviorError,
    /// Emits a warning if two values disagree. The result value will be the operand for the flag from the first module being linked.
    LLVMModuleFlagBehaviorWarning,
    /// Adds a requirement that another module flag be present and have a specified value after linking is performed. The value must be a metadata pair, where the first element of the pair is the ID of the module flag to be restricted, and the second element of the pair is the value the module flag should be restricted to. This behavior can be used to restrict the allowable results (via triggering of an error) of linking IDs with the **Override** behavior.
    LLVMModuleFlagBehaviorRequire,
    /// Uses the specified value, regardless of the behavior or value of the other module. If both modules specify **Override**, but the values differ, an error will be emitted.
    LLVMModuleFlagBehaviorOverride,
    /// Appends the two values, which are required to be metadata nodes.
    LLVMModuleFlagBehaviorAppend,
    /// Appends the two values, which are required to be metadata nodes. However, duplicate entries in the second list are dropped during the append operation.
    LLVMModuleFlagBehaviorAppendUnique,
}

pub const LLVMAttributeReturnIndex: ::libc::c_uint = 0;
pub const LLVMAttributeFunctionIndex: ::libc::c_uint = !0; // -1
/// Either LLVMAttributeReturnIndex, LLVMAttributeFunctionIndex, or a parameter
/// number from 1 to N.
pub type LLVMAttributeIndex = ::libc::c_uint;

/// Tail call kind for LLVMSetTailCallKind and LLVMGetTailCallKind.
///
/// Note that `musttail` implies `tail`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMTailCallKind {
    LLVMTailCallKindNone = 0,
    LLVMTailCallKindTail = 1,
    LLVMTailCallKindMustTail = 2,
    LLVMTailCallKindNoTail = 3,
}

pub const LLVMFastMathAllowReassoc: ::libc::c_uint = 1 << 0;
pub const LLVMFastMathNoNaNs: ::libc::c_uint = 1 << 1;
pub const LLVMFastMathNoInfs: ::libc::c_uint = 1 << 2;
pub const LLVMFastMathNoSignedZeros: ::libc::c_uint = 1 << 3;
pub const LLVMFastMathAllowReciprocal: ::libc::c_uint = 1 << 4;
pub const LLVMFastMathAllowContract: ::libc::c_uint = 1 << 5;
pub const LLVMFastMathApproxFunc: ::libc::c_uint = 1 << 6;
pub const LLVMFastMathNone: ::libc::c_uint = 0;
pub const LLVMFastMathAll: ::libc::c_uint = LLVMFastMathAllowReassoc
    | LLVMFastMathNoNaNs
    | LLVMFastMathNoInfs
    | LLVMFastMathNoSignedZeros
    | LLVMFastMathAllowReciprocal
    | LLVMFastMathAllowContract
    | LLVMFastMathApproxFunc;

/// Flags to indicate what fast-math-style optimizations are allowed on operations.
///
/// See <https://llvm.org/docs/LangRef.html#fast-math-flags>
pub type LLVMFastMathFlags = ::libc::c_uint;

/// Flags that constrain the allowed wrap semantics of a gelementptr instruction.
pub type LLVMGEPNoWrapFlags = ::libc::c_uint;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMDbgRecordKind {
    LLVMDbgRecordLabel,
    LLVMDbgRecordDeclare,
    LLVMDbgRecordValue,
    LLVMDbgRecordAssign,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LLVMDenormalModeKind {
    LLVMDenormalModeKindIEEE = 0,
    LLVMDenormalModeKindPreserveSign = 1,
    LLVMDenormalModeKindPositiveZero = 2,
    LLVMDenormalModeKindDynamic = 3,
}

pub const LLVMGEPFlagInBounds: LLVMGEPNoWrapFlags = 1;
pub const LLVMGEPFlagNUSW: LLVMGEPNoWrapFlags = 1 << 1;
pub const LLVMGEPFlagNUW: LLVMGEPNoWrapFlags = 1 << 2;

pub type LLVMDiagnosticHandler =
    Option<extern "C" fn(arg1: LLVMDiagnosticInfoRef, arg2: *mut ::libc::c_void)>;
pub type LLVMYieldCallback = Option<extern "C" fn(arg1: LLVMContextRef, arg2: *mut ::libc::c_void)>;

#[cfg(all(not(doc), not(feature = "no-llvm-linking"), LLVM_SYS_NOT_FOUND))]
std::compile_error!(concat!(
    "No suitable version of LLVM was found system-wide or pointed
       to by LLVM_SYS_",
    env!("CARGO_PKG_VERSION_MAJOR"),
    "_PREFIX.

       Consider using `llvmenv` to compile an appropriate copy of LLVM, and
       refer to the llvm-sys documentation for more information.

       llvm-sys: https://crates.io/crates/llvm-sys
       llvmenv: https://crates.io/crates/llvmenv"
));
