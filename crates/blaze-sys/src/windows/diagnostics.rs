//! Another process's memory, a thread's registers and the unwinder, and the system's thread
//! list: `Win32::System::Diagnostics::Debug`, `Win32::System::Diagnostics::ToolHelp`, and the
//! two `Win32::System::Kernel` items the unwinder's signature names (`EXCEPTION_ROUTINE`,
//! `EXCEPTION_DISPOSITION`), declared from the Win32 API reference (winnt.h, memoryapi.h,
//! processthreadsapi.h, tlhelp32.h) as windows-sys 0.61.2 has them.
//!
//! `CONTEXT` and the unwinder's tables are the processor's: x86-64 and arm64 each have their
//! own declaration and their own layout asserts.

use core::ffi::c_void;

use super::{Bool, Handle, NTSTATUS};

// ---- Win32::System::Diagnostics::Debug: aliases and constants --------------------------------

/// `CONTEXT_FLAGS` (winnt.h): which parts of a [`CONTEXT`] are valid.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_FLAGS
pub type CONTEXT_FLAGS = u32;
/// `RTL_VIRTUAL_UNWIND_HANDLER_TYPE` (winnt.h): which handler `RtlVirtualUnwind` looks for.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::RTL_VIRTUAL_UNWIND_HANDLER_TYPE
pub type RTL_VIRTUAL_UNWIND_HANDLER_TYPE = u32;
/// `UNW_FLAG_NHANDLER` (0): no handler, a plain unwind of one frame.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::UNW_FLAG_NHANDLER
pub const UNW_FLAG_NHANDLER: RTL_VIRTUAL_UNWIND_HANDLER_TYPE = 0u32;

// ---- Win32::System::Kernel ---------------------------------------------------------------------

/// `EXCEPTION_DISPOSITION` (excpt.h): what an exception routine answers.
///
/// replaces windows_sys::Win32::System::Kernel::EXCEPTION_DISPOSITION
pub type EXCEPTION_DISPOSITION = i32;
/// `EXCEPTION_ROUTINE` (winnt.h): a frame's language handler, what `RtlVirtualUnwind` returns
/// (`None` for a frame without one, or with `UNW_FLAG_NHANDLER`).
///
/// replaces windows_sys::Win32::System::Kernel::EXCEPTION_ROUTINE
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub type EXCEPTION_ROUTINE = Option<
    unsafe extern "system" fn(
        exceptionrecord: *mut EXCEPTION_RECORD,
        establisherframe: *const c_void,
        contextrecord: *mut CONTEXT,
        dispatchercontext: *const c_void,
    ) -> EXCEPTION_DISPOSITION,
>;

// ---- Win32::System::Diagnostics::Debug: structures, every processor ---------------------------

/// `M128A` (winnt.h): one 128-bit register image, low half first. windows-sys gives it the
/// alignment of its halves (8), not the header's 16.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::M128A
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct M128A {
    pub Low: u64,
    pub High: i64,
}

/// `EXCEPTION_RECORD` (winnt.h).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_RECORD
#[derive(Clone, Copy)]
#[repr(C)]
pub struct EXCEPTION_RECORD {
    pub ExceptionCode: NTSTATUS,
    pub ExceptionFlags: u32,
    pub ExceptionRecord: *mut EXCEPTION_RECORD,
    pub ExceptionAddress: *mut c_void,
    pub NumberParameters: u32,
    pub ExceptionInformation: [usize; 15],
}

/// `IMAGE_RUNTIME_FUNCTION_ENTRY` (winnt.h): one x64 `.pdata` entry, image-relative.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::IMAGE_RUNTIME_FUNCTION_ENTRY
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IMAGE_RUNTIME_FUNCTION_ENTRY {
    pub BeginAddress: u32,
    pub EndAddress: u32,
    pub Anonymous: IMAGE_RUNTIME_FUNCTION_ENTRY_0,
}

/// `IMAGE_RUNTIME_FUNCTION_ENTRY`'s unnamed union (winnt.h).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::IMAGE_RUNTIME_FUNCTION_ENTRY_0
#[derive(Clone, Copy)]
#[repr(C)]
pub union IMAGE_RUNTIME_FUNCTION_ENTRY_0 {
    pub UnwindInfoAddress: u32,
    pub UnwindData: u32,
}

/// `IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY` (winnt.h): one arm64 `.pdata` entry.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY {
    pub BeginAddress: u32,
    pub Anonymous: IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY_0,
}

/// `IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY`'s unnamed union (winnt.h): the unwind data's RVA, or the
/// packed unwind data.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY_0
#[derive(Clone, Copy)]
#[repr(C)]
pub union IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY_0 {
    pub UnwindData: u32,
    pub Anonymous: IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY_0_0,
}

/// The packed unwind data's bit fields (winnt.h).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY_0_0
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY_0_0 {
    pub _bitfield: u32,
}

/// `ARM64_NT_NEON128` (winnt.h): one arm64 vector register image.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::ARM64_NT_NEON128
#[derive(Clone, Copy)]
#[repr(C)]
pub union ARM64_NT_NEON128 {
    pub Anonymous: ARM64_NT_NEON128_0,
    pub D: [f64; 2],
    pub S: [f32; 4],
    pub H: [u16; 8],
    pub B: [u8; 16],
}

/// `ARM64_NT_NEON128`'s halves (winnt.h).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::ARM64_NT_NEON128_0
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct ARM64_NT_NEON128_0 {
    pub Low: u64,
    pub High: i64,
}

/// `XSAVE_FORMAT` (winnt.h): the legacy `fxsave` area, 64-bit form (16 XMM registers).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::XSAVE_FORMAT
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct XSAVE_FORMAT {
    pub ControlWord: u16,
    pub StatusWord: u16,
    pub TagWord: u8,
    pub Reserved1: u8,
    pub ErrorOpcode: u16,
    pub ErrorOffset: u32,
    pub ErrorSelector: u16,
    pub Reserved2: u16,
    pub DataOffset: u32,
    pub DataSelector: u16,
    pub Reserved3: u16,
    pub MxCsr: u32,
    pub MxCsr_Mask: u32,
    pub FloatRegisters: [M128A; 8],
    pub XmmRegisters: [M128A; 16],
    pub Reserved4: [u8; 96],
}

/// `UNWIND_HISTORY_TABLE` (winnt.h): `RtlLookupFunctionEntry`'s optional cache.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::UNWIND_HISTORY_TABLE
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct UNWIND_HISTORY_TABLE {
    pub Count: u32,
    pub LocalHint: u8,
    pub GlobalHint: u8,
    pub Search: u8,
    pub Once: u8,
    pub LowAddress: usize,
    pub HighAddress: usize,
    pub Entry: [UNWIND_HISTORY_TABLE_ENTRY; 12],
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
impl Default for UNWIND_HISTORY_TABLE {
    fn default() -> Self {
        // SAFETY: integers and raw pointers only; all zero is a value of each (an empty table).
        unsafe { core::mem::zeroed() }
    }
}

// ---- x86-64 --------------------------------------------------------------------------------------

/// `CONTEXT` (winnt.h, x64): a thread's registers. windows-sys aligns it to 8; the header's
/// `DECLSPEC_ALIGN(16)` is not kept, so `RtlCaptureContext`, which stores the vector registers
/// with aligned stores, needs a caller's 16-byte aligned wrapper.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::CONTEXT
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CONTEXT {
    pub P1Home: u64,
    pub P2Home: u64,
    pub P3Home: u64,
    pub P4Home: u64,
    pub P5Home: u64,
    pub P6Home: u64,
    pub ContextFlags: CONTEXT_FLAGS,
    pub MxCsr: u32,
    pub SegCs: u16,
    pub SegDs: u16,
    pub SegEs: u16,
    pub SegFs: u16,
    pub SegGs: u16,
    pub SegSs: u16,
    pub EFlags: u32,
    pub Dr0: u64,
    pub Dr1: u64,
    pub Dr2: u64,
    pub Dr3: u64,
    pub Dr6: u64,
    pub Dr7: u64,
    pub Rax: u64,
    pub Rcx: u64,
    pub Rdx: u64,
    pub Rbx: u64,
    pub Rsp: u64,
    pub Rbp: u64,
    pub Rsi: u64,
    pub Rdi: u64,
    pub R8: u64,
    pub R9: u64,
    pub R10: u64,
    pub R11: u64,
    pub R12: u64,
    pub R13: u64,
    pub R14: u64,
    pub R15: u64,
    pub Rip: u64,
    pub Anonymous: CONTEXT_0,
    pub VectorRegister: [M128A; 26],
    pub VectorControl: u64,
    pub DebugControl: u64,
    pub LastBranchToRip: u64,
    pub LastBranchFromRip: u64,
    pub LastExceptionToRip: u64,
    pub LastExceptionFromRip: u64,
}

/// `CONTEXT`'s floating-point area (winnt.h, x64): the `fxsave` image, or the XMM registers.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_0
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub union CONTEXT_0 {
    pub FltSave: XSAVE_FORMAT,
    pub Anonymous: CONTEXT_0_0,
}

/// `CONTEXT`'s XMM registers (winnt.h, x64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_0_0
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CONTEXT_0_0 {
    pub Header: [M128A; 2],
    pub Legacy: [M128A; 8],
    pub Xmm0: M128A,
    pub Xmm1: M128A,
    pub Xmm2: M128A,
    pub Xmm3: M128A,
    pub Xmm4: M128A,
    pub Xmm5: M128A,
    pub Xmm6: M128A,
    pub Xmm7: M128A,
    pub Xmm8: M128A,
    pub Xmm9: M128A,
    pub Xmm10: M128A,
    pub Xmm11: M128A,
    pub Xmm12: M128A,
    pub Xmm13: M128A,
    pub Xmm14: M128A,
    pub Xmm15: M128A,
}

/// `UNWIND_HISTORY_TABLE_ENTRY` (winnt.h, x64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::UNWIND_HISTORY_TABLE_ENTRY
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct UNWIND_HISTORY_TABLE_ENTRY {
    pub ImageBase: usize,
    pub FunctionEntry: *mut IMAGE_RUNTIME_FUNCTION_ENTRY,
}

/// `KNONVOLATILE_CONTEXT_POINTERS` (winnt.h, x64): where an unwind found each saved register.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::KNONVOLATILE_CONTEXT_POINTERS
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct KNONVOLATILE_CONTEXT_POINTERS {
    pub Anonymous1: KNONVOLATILE_CONTEXT_POINTERS_0,
    pub Anonymous2: KNONVOLATILE_CONTEXT_POINTERS_1,
}

/// The XMM registers' places (winnt.h, x64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::KNONVOLATILE_CONTEXT_POINTERS_0
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub union KNONVOLATILE_CONTEXT_POINTERS_0 {
    pub FloatingContext: [*mut M128A; 16],
    pub Anonymous: KNONVOLATILE_CONTEXT_POINTERS_0_0,
}

/// The XMM registers' places, by name (winnt.h, x64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::KNONVOLATILE_CONTEXT_POINTERS_0_0
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct KNONVOLATILE_CONTEXT_POINTERS_0_0 {
    pub Xmm0: *mut M128A,
    pub Xmm1: *mut M128A,
    pub Xmm2: *mut M128A,
    pub Xmm3: *mut M128A,
    pub Xmm4: *mut M128A,
    pub Xmm5: *mut M128A,
    pub Xmm6: *mut M128A,
    pub Xmm7: *mut M128A,
    pub Xmm8: *mut M128A,
    pub Xmm9: *mut M128A,
    pub Xmm10: *mut M128A,
    pub Xmm11: *mut M128A,
    pub Xmm12: *mut M128A,
    pub Xmm13: *mut M128A,
    pub Xmm14: *mut M128A,
    pub Xmm15: *mut M128A,
}

/// The integer registers' places (winnt.h, x64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::KNONVOLATILE_CONTEXT_POINTERS_1
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub union KNONVOLATILE_CONTEXT_POINTERS_1 {
    pub IntegerContext: [*mut u64; 16],
    pub Anonymous: KNONVOLATILE_CONTEXT_POINTERS_1_0,
}

/// The integer registers' places, by name (winnt.h, x64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::KNONVOLATILE_CONTEXT_POINTERS_1_0
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct KNONVOLATILE_CONTEXT_POINTERS_1_0 {
    pub Rax: *mut u64,
    pub Rcx: *mut u64,
    pub Rdx: *mut u64,
    pub Rbx: *mut u64,
    pub Rsp: *mut u64,
    pub Rbp: *mut u64,
    pub Rsi: *mut u64,
    pub Rdi: *mut u64,
    pub R8: *mut u64,
    pub R9: *mut u64,
    pub R10: *mut u64,
    pub R11: *mut u64,
    pub R12: *mut u64,
    pub R13: *mut u64,
    pub R14: *mut u64,
    pub R15: *mut u64,
}

// winnt.h, x64 (as windows-sys lays it out: `M128A` and `CONTEXT` aligned to 8).
#[cfg(target_arch = "x86_64")]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<XSAVE_FORMAT>() == 512 && align_of::<XSAVE_FORMAT>() == 8);
    assert!(offset_of!(XSAVE_FORMAT, MxCsr) == 24);
    assert!(offset_of!(XSAVE_FORMAT, FloatRegisters) == 32);
    assert!(offset_of!(XSAVE_FORMAT, XmmRegisters) == 160);
    assert!(offset_of!(XSAVE_FORMAT, Reserved4) == 416);
    assert!(size_of::<CONTEXT_0_0>() == 416);
    assert!(offset_of!(CONTEXT_0_0, Xmm0) == 160);
    assert!(size_of::<CONTEXT_0>() == 512);
    assert!(size_of::<CONTEXT>() == 1232 && align_of::<CONTEXT>() == 8);
    assert!(offset_of!(CONTEXT, ContextFlags) == 48);
    assert!(offset_of!(CONTEXT, MxCsr) == 52);
    assert!(offset_of!(CONTEXT, SegCs) == 56);
    assert!(offset_of!(CONTEXT, EFlags) == 68);
    assert!(offset_of!(CONTEXT, Dr0) == 72);
    assert!(offset_of!(CONTEXT, Rax) == 120);
    assert!(offset_of!(CONTEXT, Rsp) == 152);
    assert!(offset_of!(CONTEXT, Rbp) == 160);
    assert!(offset_of!(CONTEXT, R15) == 240);
    assert!(offset_of!(CONTEXT, Rip) == 248);
    assert!(offset_of!(CONTEXT, Anonymous) == 256);
    assert!(offset_of!(CONTEXT, VectorRegister) == 768);
    assert!(offset_of!(CONTEXT, VectorControl) == 1184);
    assert!(offset_of!(CONTEXT, LastExceptionFromRip) == 1224);
    assert!(size_of::<UNWIND_HISTORY_TABLE_ENTRY>() == 16);
    assert!(offset_of!(UNWIND_HISTORY_TABLE_ENTRY, FunctionEntry) == 8);
    assert!(size_of::<KNONVOLATILE_CONTEXT_POINTERS_0>() == 128);
    assert!(size_of::<KNONVOLATILE_CONTEXT_POINTERS_1>() == 128);
    assert!(size_of::<KNONVOLATILE_CONTEXT_POINTERS>() == 256);
    assert!(offset_of!(KNONVOLATILE_CONTEXT_POINTERS, Anonymous2) == 128);
};

// ---- arm64 ---------------------------------------------------------------------------------------

/// `CONTEXT` (winnt.h, ARM64): a thread's registers. windows-sys aligns it to 8; the header's
/// `DECLSPEC_ALIGN(16)` is not kept.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::CONTEXT
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CONTEXT {
    pub ContextFlags: CONTEXT_FLAGS,
    pub Cpsr: u32,
    pub Anonymous: CONTEXT_0,
    pub Sp: u64,
    pub Pc: u64,
    pub V: [ARM64_NT_NEON128; 32],
    pub Fpcr: u32,
    pub Fpsr: u32,
    pub Bcr: [u32; 8],
    pub Bvr: [u64; 8],
    pub Wcr: [u32; 2],
    pub Wvr: [u64; 2],
}

/// `CONTEXT`'s general registers (winnt.h, ARM64): by name, or `X[0..31]`.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_0
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub union CONTEXT_0 {
    pub Anonymous: CONTEXT_0_0,
    pub X: [u64; 31],
}

/// `CONTEXT`'s general registers by name (winnt.h, ARM64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::CONTEXT_0_0
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct CONTEXT_0_0 {
    pub X0: u64,
    pub X1: u64,
    pub X2: u64,
    pub X3: u64,
    pub X4: u64,
    pub X5: u64,
    pub X6: u64,
    pub X7: u64,
    pub X8: u64,
    pub X9: u64,
    pub X10: u64,
    pub X11: u64,
    pub X12: u64,
    pub X13: u64,
    pub X14: u64,
    pub X15: u64,
    pub X16: u64,
    pub X17: u64,
    pub X18: u64,
    pub X19: u64,
    pub X20: u64,
    pub X21: u64,
    pub X22: u64,
    pub X23: u64,
    pub X24: u64,
    pub X25: u64,
    pub X26: u64,
    pub X27: u64,
    pub X28: u64,
    pub Fp: u64,
    pub Lr: u64,
}

/// `UNWIND_HISTORY_TABLE_ENTRY` (winnt.h, ARM64).
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::UNWIND_HISTORY_TABLE_ENTRY
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct UNWIND_HISTORY_TABLE_ENTRY {
    pub ImageBase: usize,
    pub FunctionEntry: *mut IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY,
}

/// `KNONVOLATILE_CONTEXT_POINTERS` (winnt.h, ARM64): where an unwind found each callee-saved
/// register.
///
/// replaces windows_sys::Win32::System::Diagnostics::Debug::KNONVOLATILE_CONTEXT_POINTERS
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct KNONVOLATILE_CONTEXT_POINTERS {
    pub X19: *mut u64,
    pub X20: *mut u64,
    pub X21: *mut u64,
    pub X22: *mut u64,
    pub X23: *mut u64,
    pub X24: *mut u64,
    pub X25: *mut u64,
    pub X26: *mut u64,
    pub X27: *mut u64,
    pub X28: *mut u64,
    pub Fp: *mut u64,
    pub Lr: *mut u64,
    pub D8: *mut u64,
    pub D9: *mut u64,
    pub D10: *mut u64,
    pub D11: *mut u64,
    pub D12: *mut u64,
    pub D13: *mut u64,
    pub D14: *mut u64,
    pub D15: *mut u64,
}

// winnt.h, ARM64 (as windows-sys lays it out: `CONTEXT` aligned to 8; 0x390 bytes).
#[cfg(target_arch = "aarch64")]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<ARM64_NT_NEON128>() == 16 && align_of::<ARM64_NT_NEON128>() == 8);
    assert!(size_of::<CONTEXT_0_0>() == 248);
    assert!(offset_of!(CONTEXT_0_0, Fp) == 232);
    assert!(offset_of!(CONTEXT_0_0, Lr) == 240);
    assert!(size_of::<CONTEXT_0>() == 248);
    assert!(size_of::<CONTEXT>() == 912 && align_of::<CONTEXT>() == 8);
    assert!(offset_of!(CONTEXT, Cpsr) == 4);
    assert!(offset_of!(CONTEXT, Anonymous) == 8);
    assert!(offset_of!(CONTEXT, Sp) == 256);
    assert!(offset_of!(CONTEXT, Pc) == 264);
    assert!(offset_of!(CONTEXT, V) == 272);
    assert!(offset_of!(CONTEXT, Fpcr) == 784);
    assert!(offset_of!(CONTEXT, Fpsr) == 788);
    assert!(offset_of!(CONTEXT, Bcr) == 792);
    assert!(offset_of!(CONTEXT, Bvr) == 824);
    assert!(offset_of!(CONTEXT, Wcr) == 888);
    assert!(offset_of!(CONTEXT, Wvr) == 896);
    assert!(size_of::<UNWIND_HISTORY_TABLE_ENTRY>() == 16);
    assert!(offset_of!(UNWIND_HISTORY_TABLE_ENTRY, FunctionEntry) == 8);
    assert!(size_of::<KNONVOLATILE_CONTEXT_POINTERS>() == 160);
    assert!(offset_of!(KNONVOLATILE_CONTEXT_POINTERS, D8) == 96);
};

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
impl Default for CONTEXT {
    fn default() -> Self {
        // SAFETY: integers, and unions and arrays of integers, only; all zero is a value of each.
        unsafe { core::mem::zeroed() }
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
impl Default for KNONVOLATILE_CONTEXT_POINTERS {
    fn default() -> Self {
        // SAFETY: raw pointers only (in unions, on x64); all zero is null, a value of each.
        unsafe { core::mem::zeroed() }
    }
}

// winnt.h: every processor (8-byte pointers on x86-64 and arm64).
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<M128A>() == 16 && align_of::<M128A>() == 8);
    assert!(offset_of!(M128A, High) == 8);
    assert!(size_of::<EXCEPTION_RECORD>() == 152 && align_of::<EXCEPTION_RECORD>() == 8);
    assert!(offset_of!(EXCEPTION_RECORD, ExceptionRecord) == 8);
    assert!(offset_of!(EXCEPTION_RECORD, ExceptionAddress) == 16);
    assert!(offset_of!(EXCEPTION_RECORD, NumberParameters) == 24);
    assert!(offset_of!(EXCEPTION_RECORD, ExceptionInformation) == 32);
    assert!(size_of::<IMAGE_RUNTIME_FUNCTION_ENTRY>() == 12);
    assert!(align_of::<IMAGE_RUNTIME_FUNCTION_ENTRY>() == 4);
    assert!(offset_of!(IMAGE_RUNTIME_FUNCTION_ENTRY, EndAddress) == 4);
    assert!(offset_of!(IMAGE_RUNTIME_FUNCTION_ENTRY, Anonymous) == 8);
    assert!(size_of::<IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY>() == 8);
    assert!(align_of::<IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY>() == 4);
    assert!(offset_of!(IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY, Anonymous) == 4);
    assert!(size_of::<UNWIND_HISTORY_TABLE>() == 216 && align_of::<UNWIND_HISTORY_TABLE>() == 8);
    assert!(offset_of!(UNWIND_HISTORY_TABLE, LocalHint) == 4);
    assert!(offset_of!(UNWIND_HISTORY_TABLE, LowAddress) == 8);
    assert!(offset_of!(UNWIND_HISTORY_TABLE, HighAddress) == 16);
    assert!(offset_of!(UNWIND_HISTORY_TABLE, Entry) == 24);
};

// ---- Win32::System::Diagnostics::ToolHelp ------------------------------------------------------

/// `CREATE_TOOLHELP_SNAPSHOT_FLAGS` (tlhelp32.h): what a snapshot lists.
///
/// replaces windows_sys::Win32::System::Diagnostics::ToolHelp::CREATE_TOOLHELP_SNAPSHOT_FLAGS
pub type CREATE_TOOLHELP_SNAPSHOT_FLAGS = u32;
/// `TH32CS_SNAPTHREAD` (4): every thread of the system.
///
/// replaces windows_sys::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPTHREAD
pub const TH32CS_SNAPTHREAD: CREATE_TOOLHELP_SNAPSHOT_FLAGS = 4u32;

/// `THREADENTRY32` (tlhelp32.h): one thread of a snapshot; `dwSize` is set by the caller.
///
/// replaces windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct THREADENTRY32 {
    pub dwSize: u32,
    pub cntUsage: u32,
    pub th32ThreadID: u32,
    pub th32OwnerProcessID: u32,
    pub tpBasePri: i32,
    pub tpDeltaPri: i32,
    pub dwFlags: u32,
}

// tlhelp32.h: the same on every Windows target (`DWORD`s and `LONG`s).
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<THREADENTRY32>() == 28 && align_of::<THREADENTRY32>() == 4);
    assert!(offset_of!(THREADENTRY32, th32ThreadID) == 8);
    assert!(offset_of!(THREADENTRY32, th32OwnerProcessID) == 12);
    assert!(offset_of!(THREADENTRY32, dwFlags) == 24);
};

// ---- Functions -------------------------------------------------------------------------------

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `FlushInstructionCache` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache
    pub fn FlushInstructionCache(hprocess: Handle, lpbaseaddress: *const c_void, dwsize: usize) -> Bool;

    /// `ReadProcessMemory` (memoryapi.h): `nsize` bytes of `hprocess` at `lpbaseaddress` into
    /// `lpbuffer`; the bytes read into `lpnumberofbytesread`.
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory
    pub fn ReadProcessMemory(
        hprocess: Handle,
        lpbaseaddress: *const c_void,
        lpbuffer: *mut c_void,
        nsize: usize,
        lpnumberofbytesread: *mut usize,
    ) -> Bool;

    /// `WriteProcessMemory` (memoryapi.h).
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory
    pub fn WriteProcessMemory(
        hprocess: Handle,
        lpbaseaddress: *const c_void,
        lpbuffer: *const c_void,
        nsize: usize,
        lpnumberofbyteswritten: *mut usize,
    ) -> Bool;

    /// `RtlCaptureContext` (winnt.h): the calling thread's registers into `contextrecord`.
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureContext
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    pub fn RtlCaptureContext(contextrecord: *mut CONTEXT);

    /// `RtlLookupFunctionEntry` (winnt.h, x64): the `.pdata` entry covering `controlpc`, and
    /// its image's base into `imagebase`; null for a leaf function or no image.
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry
    #[cfg(target_arch = "x86_64")]
    pub fn RtlLookupFunctionEntry(
        controlpc: u64,
        imagebase: *mut u64,
        historytable: *mut UNWIND_HISTORY_TABLE,
    ) -> *mut IMAGE_RUNTIME_FUNCTION_ENTRY;

    /// `RtlLookupFunctionEntry` (winnt.h, ARM64).
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry
    #[cfg(target_arch = "aarch64")]
    pub fn RtlLookupFunctionEntry(
        controlpc: usize,
        imagebase: *mut usize,
        historytable: *mut UNWIND_HISTORY_TABLE,
    ) -> *mut IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY;

    /// `RtlVirtualUnwind` (winnt.h, x64): unwinds `contextrecord` by one frame.
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::RtlVirtualUnwind
    #[cfg(target_arch = "x86_64")]
    pub fn RtlVirtualUnwind(
        handlertype: RTL_VIRTUAL_UNWIND_HANDLER_TYPE,
        imagebase: u64,
        controlpc: u64,
        functionentry: *const IMAGE_RUNTIME_FUNCTION_ENTRY,
        contextrecord: *mut CONTEXT,
        handlerdata: *mut *mut c_void,
        establisherframe: *mut u64,
        contextpointers: *mut KNONVOLATILE_CONTEXT_POINTERS,
    ) -> EXCEPTION_ROUTINE;

    /// `RtlVirtualUnwind` (winnt.h, ARM64).
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::Debug::RtlVirtualUnwind
    #[cfg(target_arch = "aarch64")]
    pub fn RtlVirtualUnwind(
        handlertype: RTL_VIRTUAL_UNWIND_HANDLER_TYPE,
        imagebase: usize,
        controlpc: usize,
        functionentry: *const IMAGE_ARM64_RUNTIME_FUNCTION_ENTRY,
        contextrecord: *mut CONTEXT,
        handlerdata: *mut *mut c_void,
        establisherframe: *mut usize,
        contextpointers: *mut KNONVOLATILE_CONTEXT_POINTERS,
    ) -> EXCEPTION_ROUTINE;

    /// `CreateToolhelp32Snapshot` (tlhelp32.h): [`Handle::INVALID`] on failure.
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot
    pub fn CreateToolhelp32Snapshot(dwflags: CREATE_TOOLHELP_SNAPSHOT_FLAGS, th32processid: u32) -> Handle;

    /// `Thread32First` (tlhelp32.h): false with `ERROR_NO_MORE_FILES` for an empty snapshot.
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32First
    pub fn Thread32First(hsnapshot: Handle, lpte: *mut THREADENTRY32) -> Bool;

    /// `Thread32Next` (tlhelp32.h): false with `ERROR_NO_MORE_FILES` past the last.
    ///
    /// replaces windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32Next
    pub fn Thread32Next(hsnapshot: Handle, lpte: *mut THREADENTRY32) -> Bool;
}
