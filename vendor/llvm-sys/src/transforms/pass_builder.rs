#![allow(non_snake_case)]

use crate::error::LLVMErrorRef;
use crate::prelude::*;
use crate::target_machine::LLVMTargetMachineRef;

#[derive(Debug)]
pub enum LLVMOpaquePassBuilderOptions {}
pub type LLVMPassBuilderOptionsRef = *mut LLVMOpaquePassBuilderOptions;

extern "C" {
    pub fn LLVMRunPasses(
        M: LLVMModuleRef,
        Passes: *const ::libc::c_char,
        TM: LLVMTargetMachineRef,
        Options: LLVMPassBuilderOptionsRef,
    ) -> LLVMErrorRef;
    /// Construct and run a set of passes over a function.
    ///
    /// This function behaves the same as LLVMRunPasses, but operates on a single
    /// function instead of an entire module.
    pub fn LLVMRunPassesOnFunction(
        F: LLVMValueRef,
        Passes: *const ::libc::c_char,
        TM: LLVMTargetMachineRef,
        Options: LLVMPassBuilderOptionsRef,
    ) -> LLVMErrorRef;
    pub fn LLVMCreatePassBuilderOptions() -> LLVMPassBuilderOptionsRef;
    pub fn LLVMPassBuilderOptionsSetVerifyEach(
        Options: LLVMPassBuilderOptionsRef,
        VerifyEach: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetDebugLogging(
        Options: LLVMPassBuilderOptionsRef,
        DebugLogging: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetAAPipeline(
        Options: LLVMPassBuilderOptionsRef,
        AAPipeline: *const ::libc::c_char,
    );
    pub fn LLVMPassBuilderOptionsSetLoopInterleaving(
        Options: LLVMPassBuilderOptionsRef,
        LoopInterleaving: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetLoopVectorization(
        Options: LLVMPassBuilderOptionsRef,
        LoopVectorization: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetSLPVectorization(
        Options: LLVMPassBuilderOptionsRef,
        SLPVectorization: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetLoopUnrolling(
        Options: LLVMPassBuilderOptionsRef,
        LoopUnrolling: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetForgetAllSCEVInLoopUnroll(
        Options: LLVMPassBuilderOptionsRef,
        ForgetAllSCEVInLoopUnroll: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetLicmMssaOptCap(
        Options: LLVMPassBuilderOptionsRef,
        LicmMssaOptCap: ::libc::c_uint,
    );
    pub fn LLVMPassBuilderOptionsSetLicmMssaNoAccForPromotionCap(
        Options: LLVMPassBuilderOptionsRef,
        LicmMssaNoAccForPromotionCap: ::libc::c_uint,
    );
    pub fn LLVMPassBuilderOptionsSetCallGraphProfile(
        Options: LLVMPassBuilderOptionsRef,
        CallGraphProfile: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetMergeFunctions(
        Options: LLVMPassBuilderOptionsRef,
        MergeFunctions: LLVMBool,
    );
    pub fn LLVMPassBuilderOptionsSetInlinerThreshold(
        Options: LLVMPassBuilderOptionsRef,
        Threshold: ::libc::c_int,
    );
    pub fn LLVMDisposePassBuilderOptions(Options: LLVMPassBuilderOptionsRef);
}

/// blaze: LLVM's pass builder, analysis managers, instrumentation and parsed pipelines kept for
/// one target machine (wrappers/passes.cpp). Used by one thread at a time, with modules of one
/// context, and disposed of before the target machine and the context.
#[derive(Debug)]
pub enum LLVMBlazeOpaquePasses {}
pub type LLVMBlazePassesRef = *mut LLVMBlazeOpaquePasses;

extern "C" {
    pub fn LLVMBlazeCreatePasses(TM: LLVMTargetMachineRef) -> LLVMBlazePassesRef;
    /// Whether the target of `TM` has a feature named `Name` (`Length` bytes, no NUL needed),
    /// as rustc's `LLVMRustHasFeature` asks. LLVM told a name it lacks warns and ignores it.
    pub fn LLVMBlazeHasFeature(TM: LLVMTargetMachineRef, Name: *const ::libc::c_char, Length: usize) -> LLVMBool;
    /// Whether the target of `TM` knows the CPU `Name` (`Length` bytes, no NUL needed), its
    /// aliases included. LLVM told a CPU it does not know warns and compiles for none.
    pub fn LLVMBlazeHasCpu(TM: LLVMTargetMachineRef, Name: *const ::libc::c_char, Length: usize) -> LLVMBool;
    pub fn LLVMBlazeDisposePasses(Passes: LLVMBlazePassesRef);
    /// Runs the pipeline `Pipeline` (`Length` bytes, no NUL needed) over `M`, as
    /// [`LLVMRunPasses`] does with default options but `SLPVectorization`.
    pub fn LLVMBlazeRunPasses(
        Passes: LLVMBlazePassesRef,
        M: LLVMModuleRef,
        Pipeline: *const ::libc::c_char,
        Length: usize,
        SLPVectorization: LLVMBool,
    ) -> LLVMErrorRef;
    /// Instruments `M` for `Wanted`, which is read during the call only: SanitizerCoverage,
    /// then MemorySanitizer, ThreadSanitizer and AddressSanitizer, as clang orders them and with
    /// rustc's options for each. `M` is of the context the passes were first run with.
    pub fn LLVMBlazeRunSanitizers(
        Passes: LLVMBlazePassesRef,
        M: LLVMModuleRef,
        Wanted: *const LLVMBlazeSanitizers,
    ) -> LLVMErrorRef;
    /// Passes whose pipelines are built with `Pgo` (read during the call only), as rustc's
    /// `LLVMRustOptimize` builds them for `-Cprofile-generate` and `-Cprofile-use`: the default
    /// pipelines [`LLVMBlazeRunPasses`] runs then run PGO's passes where LLVM puts them (not in
    /// ThinLTO's post-link one). A profile used is read from `Pgo.Profile`, which outlives the
    /// passes. Null for an `Action` that is neither [`LLVMBlazePgoGenerate`] nor
    /// [`LLVMBlazePgoUse`].
    pub fn LLVMBlazeCreatePgoPasses(
        TM: LLVMTargetMachineRef,
        Pgo: *const LLVMBlazePgo,
    ) -> LLVMBlazePassesRef;
    /// Runs PGO's passes alone over `M`, as LLVM does at `-O0`. For passes
    /// [`LLVMBlazeCreatePgoPasses`] made.
    pub fn LLVMBlazeRunPgo(Passes: LLVMBlazePassesRef, M: LLVMModuleRef) -> LLVMErrorRef;
    /// How many functions the passes found a record of in their profile under the function's
    /// name with another hash, since this was last asked.
    pub fn LLVMBlazeTakeMismatched(Passes: LLVMBlazePassesRef) -> u32;
    /// How many functions the profile the passes use gave counts to (an entry count), since
    /// this was last asked.
    pub fn LLVMBlazeTakeApplied(Passes: LLVMBlazePassesRef) -> u32;
    /// The indexed profile of IR instrumentation with `Count` records, as `llvm-profdata
    /// merge` writes it, into `*Out`, which the caller disposes of.
    pub fn LLVMBlazeWriteProfile(
        Records: *const LLVMBlazeProfileRecord,
        Count: usize,
        Out: *mut LLVMMemoryBufferRef,
    ) -> LLVMErrorRef;
    /// Starts a report: what was kept is let go, and the runs from now on catch the loop
    /// vectorizer's remarks (missed and analysis) until [`LLVMBlazeReportStop`].
    pub fn LLVMBlazeReportStart(Passes: LLVMBlazePassesRef);
    /// Ends a report: the runs from now on catch no remarks, and what was kept is let go.
    pub fn LLVMBlazeReportStop(Passes: LLVMBlazePassesRef);
    /// Adds to the report each branch out of a loop of `Fn` (a defined function of a module
    /// the passes ran) to a panic path, then gives every finding its loop's profile heat.
    pub fn LLVMBlazeReportKept(Passes: LLVMBlazePassesRef, Fn: LLVMValueRef);
    /// The findings as [`LLVMBlazeReportKept`] last made them, into `*Out`, live until the
    /// next `LLVMBlazeReport*` call on `Passes`. Returns how many.
    pub fn LLVMBlazeReportRows(Passes: LLVMBlazePassesRef, Out: *mut *const LLVMBlazeReportRow) -> usize;
}

/// blaze: a finding of the report (wrappers/passes.cpp's struct of the same name, field for
/// field). The strings point into the passes, live as [`LLVMBlazeReportRows`] says.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LLVMBlazeReportRow {
    /// [`LLVMBlazeFoundKept`], [`LLVMBlazeFoundMissed`] or [`LLVMBlazeFoundAnalysis`].
    pub Kind: u8,
    /// [`LLVMBlazeHeatUnprofiled`], [`LLVMBlazeHeatUncounted`], [`LLVMBlazeHeatCold`] or
    /// [`LLVMBlazeHeatHot`].
    pub Heat: u8,
    /// The line of the code's debug location, through inlining to the function's own; 0: none.
    pub Line: u32,
    /// The line of the loop's start; 0 where not known.
    pub LoopLine: u32,
    /// The loop header's profile count, for cold and hot.
    pub Count: u64,
    /// Kept: the callee's name (empty through a pointer). A remark: its name.
    pub Name: *const ::libc::c_char,
    pub NameLength: usize,
    /// A remark's message; empty for kept.
    pub Message: *const ::libc::c_char,
    pub MessageLength: usize,
}

/// A branch out of a loop to code that never returns: a check whose panic path survived.
pub const LLVMBlazeFoundKept: u8 = 1;
/// The loop vectorizer left a loop scalar (its "missed" remark).
pub const LLVMBlazeFoundMissed: u8 = 2;
/// Why the loop vectorizer left a loop scalar (its "analysis" remark).
pub const LLVMBlazeFoundAnalysis: u8 = 3;

/// Built without a profile.
pub const LLVMBlazeHeatUnprofiled: u8 = 0;
/// With a profile, and no count reaches the loop.
pub const LLVMBlazeHeatUncounted: u8 = 1;
/// The loop's count is not hot by the module's profile summary.
pub const LLVMBlazeHeatCold: u8 = 2;
/// The loop's count is hot by the module's profile summary.
pub const LLVMBlazeHeatHot: u8 = 3;

/// blaze: what [`LLVMBlazeCreatePgoPasses`] builds pipelines for (wrappers/passes.cpp's struct
/// of the same name, field for field).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LLVMBlazePgo {
    /// [`LLVMBlazePgoGenerate`] or [`LLVMBlazePgoUse`].
    pub Action: u8,
    /// Use: the indexed profile, `Length` bytes followed by a NUL.
    pub Profile: *const ::libc::c_char,
    pub Length: usize,
}

/// `-Cprofile-generate`: LLVM's IR instrumentation.
pub const LLVMBlazePgoGenerate: u8 = 1;
/// `-Cprofile-use`: the profile's counts as the code's branch weights.
pub const LLVMBlazePgoUse: u8 = 2;

/// blaze: one function's counts for [`LLVMBlazeWriteProfile`] (wrappers/passes.cpp's struct of
/// the same name, field for field).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LLVMBlazeProfileRecord {
    pub Name: *const ::libc::c_char,
    pub NameLength: usize,
    pub Hash: u64,
    pub Counts: *const u64,
    pub CountsLength: usize,
}

/// blaze: what [`LLVMBlazeRunSanitizers`] instruments for (wrappers/passes.cpp's struct of
/// the same name, field for field). A 0 is false.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LLVMBlazeSanitizers {
    pub Address: u8,
    pub AddressRecover: u8,
    pub Thread: u8,
    pub Memory: u8,
    pub MemoryRecover: u8,
    /// `-sanitizer-coverage-level`, 1 to 4; any other is no SanitizerCoverage.
    pub CoverageLevel: u8,
    /// The `LLVMBlazeCoverage*` bits.
    pub CoverageFlags: u16,
    /// 0, 1 or 2.
    pub MemoryTrackOrigins: i32,
}

/// The `-sanitizer-coverage-*` switches of [`LLVMBlazeSanitizers::CoverageFlags`].
pub const LLVMBlazeCoverageTracePC: u16 = 1 << 0;
pub const LLVMBlazeCoverageTracePCGuard: u16 = 1 << 1;
pub const LLVMBlazeCoverageInline8bitCounters: u16 = 1 << 2;
pub const LLVMBlazeCoverageInlineBoolFlag: u16 = 1 << 3;
pub const LLVMBlazeCoveragePCTable: u16 = 1 << 4;
pub const LLVMBlazeCoverageTraceCmp: u16 = 1 << 5;
pub const LLVMBlazeCoverageTraceDiv: u16 = 1 << 6;
pub const LLVMBlazeCoverageTraceGep: u16 = 1 << 7;
pub const LLVMBlazeCoverageTraceLoads: u16 = 1 << 8;
pub const LLVMBlazeCoverageTraceStores: u16 = 1 << 9;
pub const LLVMBlazeCoverageNoPrune: u16 = 1 << 10;
pub const LLVMBlazeCoverageStackDepth: u16 = 1 << 11;
pub const LLVMBlazeCoverageTracePCEntryExit: u16 = 1 << 12;
pub const LLVMBlazeCoverageControlFlow: u16 = 1 << 13;

const _: () = assert!(std::mem::size_of::<LLVMBlazeSanitizers>() == 12);
