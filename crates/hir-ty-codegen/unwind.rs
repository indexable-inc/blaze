//! Unwind tables (`.eh_frame`) and landing pads (`.gcc_except_table`), without which a panic
//! cannot cross a function we compiled. Ported from rustc's Cranelift backend. Windows has
//! tables of its own, which are in [`seh`].

#![forbid(unsafe_code)]

use blaze_format::cursor::Reader;
use cranelift_codegen::{
    CompiledCode, Context, MachExceptionHandler,
    gimli::{
        self, RunTimeEndian, SectionId,
        write::{
            Address, CallFrameInstruction, CieId, CommonInformationEntry, EhFrame, EndianVec,
            FrameDescriptionEntry, FrameTable, Result as WriteResult, Writer,
        },
    },
    ir::{AbiParam, Signature, types},
    isa::{TargetIsa, unwind::UnwindInfo},
};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{
    ObjectModule, ObjectProduct,
    object::{
        BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, macho,
        write::{Relocation, StandardSection},
    },
};

#[cfg(test)]
mod gcc_except_table;
mod lsda;
pub(super) mod seh;
#[cfg(test)]
mod tests;

pub(super) use blaze_codegen::unwind::FrameRule;
use lsda::CallEntry;

/// Exception tag of a landing pad that runs destructors and resumes unwinding.
pub(super) const CLEANUP: u32 = 0;
/// Exception tag of a landing pad that stops the unwind, for `catch_unwind`.
pub(super) const CATCH: u32 = 1;

/// The frame layout and the landing pads of one compiled function, which do not depend on
/// where the function ends up.
#[derive(Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(super) struct FunctionUnwind {
    frame: Frame,
    /// The function's `.gcc_except_table` entry. A function without one has no call that
    /// lands anywhere, and no personality to ask.
    landing_pads: Option<Vec<u8>>,
}

/// A function's unwind information apart from where the function is ([`FunctionUnwind::v2_parts`]).
pub(super) enum UnwindParts<'a> {
    Dwarf { fde: Fde<'a>, landing_pads: Option<&'a [u8]> },
    Windows { len: u32, info: &'a [u8], landing_pads: Option<&'a [u8]> },
}

/// How to find the caller's frame at each place in a function, as the producer of its code
/// tells it.
#[derive(Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
enum Frame {
    Cranelift(cranelift_codegen::isa::unwind::systemv::UnwindInfo),
    Rows(FrameRows),
    Windows(seh::Frame),
}

/// The rules of a function's `.eh_frame` entry, each with the offset in the function it
/// holds from.
#[derive(Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(super) struct FrameRows {
    /// How many bytes of code the rules cover.
    pub(super) len: u32,
    pub(super) rows: Vec<(u32, FrameRule)>,
}

/// What holds where a function is entered, which the entry every function shares says.
/// On x86-64 the frame is eight bytes above the stack pointer, and the return address
/// is at its bottom. On AArch64 the frame is at the stack pointer, and the return
/// address is in a register.
#[cfg(feature = "llvm")]
pub(super) fn on_entry(arch: super::Arch) -> &'static [FrameRule] {
    match arch {
        super::Arch::Aarch64 => &[FrameRule::Cfa(31, 0)],
        super::Arch::Wasm32 => &[],
        super::Arch::X86_64 => &[FrameRule::Cfa(7, 8), FrameRule::Offset(16, -8)],
    }
}

fn instruction(rule: FrameRule) -> CallFrameInstruction {
    let register = gimli::Register;
    match rule {
        FrameRule::Cfa(reg, offset) => CallFrameInstruction::Cfa(register(reg), offset),
        FrameRule::CfaRegister(reg) => CallFrameInstruction::CfaRegister(register(reg)),
        FrameRule::CfaOffset(offset) => CallFrameInstruction::CfaOffset(offset),
        FrameRule::Offset(reg, offset) => CallFrameInstruction::Offset(register(reg), offset),
        FrameRule::Restore(reg) => CallFrameInstruction::Restore(register(reg)),
        FrameRule::SameValue(reg) => CallFrameInstruction::SameValue(register(reg)),
        FrameRule::Undefined(reg) => CallFrameInstruction::Undefined(register(reg)),
        FrameRule::RememberState => CallFrameInstruction::RememberState,
        FrameRule::RestoreState => CallFrameInstruction::RestoreState,
        FrameRule::ArgsSize(size) => CallFrameInstruction::ArgsSize(size),
        FrameRule::RaSignState(signed) => {
            let mut expression = gimli::write::Expression::new();
            expression.op(if signed { gimli::DW_OP_lit1 } else { gimli::DW_OP_lit0 });
            CallFrameInstruction::ValExpression(register(34), expression)
        }
    }
}

impl FunctionUnwind {
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    pub(super) fn of(isa: &dyn TargetIsa, context: &Context) -> Result<Option<Self>, String> {
        let Some(code) = context.compiled_code() else { return Ok(None) };
        let frame = match SystemV::of(isa) {
            Some(systemv) => Frame::Rows(systemv.rows(code)?),
            // alloc: the error's message
            None => match code.create_unwind_info(isa).map_err(|e| e.to_string())? {
                Some(UnwindInfo::SystemV(info)) => Frame::Cranelift(info),
                Some(UnwindInfo::WindowsX64(info)) => {
                    let (frame, landing_pads) = seh::function(code, &info)?;
                    return Ok(Some(FunctionUnwind { frame: Frame::Windows(frame), landing_pads }));
                }
                _ => return Ok(None),
            },
        };
        let mut lands = false;
        for call_site in code.buffer.call_sites() {
            for handler in call_site.exception_handlers {
                let MachExceptionHandler::Tag(..) = handler else {
                    // alloc: the error's message
                    return Err("unexpected exception handler".to_owned());
                };
                lands = true;
            }
        }
        // Where no call lands anywhere, an unwind goes on through each to the caller, as it
        // does through a frame without the entry: its personality is not asked, and LLVM
        // writes none for such a function of rustc's either.
        let landing_pads = lands.then(|| lsda::write(|| call_entries(code)));
        Ok(Some(FunctionUnwind { frame, landing_pads }))
    }
}

/// What Cranelift's `.eh_frame` rows of a System V target of x86-64 or AArch64 depend on, as
/// its `isa/<arch>/inst/unwind/systemv.rs` register mappers say: the frame pointer, and the
/// link register with where it is saved above the frame pointer. Its `UnwindInfo` keeps the
/// rows to itself, so [`SystemV::rows`] makes them of the code's unwind instructions as its
/// `create_unwind_info_from_insts` does, once, as rules of our own that are written straight
/// into an entry ([`Fde::write`]).
#[derive(Clone, Copy)]
struct SystemV {
    arch: Arch,
    frame_pointer: u16,
    link: Option<(u16, i32)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arch {
    X86_64,
    Aarch64,
}

impl SystemV {
    /// `None` for Windows, whose frames are its own, and for an architecture whose rows
    /// Cranelift makes ([`Frame::Cranelift`]).
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    fn of(isa: &dyn TargetIsa) -> Option<SystemV> {
        use target_lexicon::{Architecture, OperatingSystem};
        if isa.triple().operating_system == OperatingSystem::Windows {
            return None;
        }
        match isa.triple().architecture {
            Architecture::X86_64 => {
                Some(SystemV { arch: Arch::X86_64, frame_pointer: gimli::X86_64::RBP.0, link: None })
            }
            Architecture::Aarch64(_) => Some(SystemV {
                arch: Arch::Aarch64,
                frame_pointer: gimli::AArch64::X29.0,
                link: Some((gimli::AArch64::X30.0, 8)),
            }),
            _ => None,
        }
    }

    /// The DWARF number of `reg`, as Cranelift's `map_reg` gives it.
    fn register(self, reg: cranelift_codegen::RealReg) -> Result<u16, String> {
        use cranelift_codegen::RegClass;
        /// x86-64's general registers by their encoding (`rax`, `rcx`, `rdx`, `rbx`, `rsp`,
        /// `rbp`, `rsi`, `rdi`, `r8` to `r15`).
        const X86_64_GENERAL: [gimli::Register; 16] = [
            gimli::X86_64::RAX,
            gimli::X86_64::RCX,
            gimli::X86_64::RDX,
            gimli::X86_64::RBX,
            gimli::X86_64::RSP,
            gimli::X86_64::RBP,
            gimli::X86_64::RSI,
            gimli::X86_64::RDI,
            gimli::X86_64::R8,
            gimli::X86_64::R9,
            gimli::X86_64::R10,
            gimli::X86_64::R11,
            gimli::X86_64::R12,
            gimli::X86_64::R13,
            gimli::X86_64::R14,
            gimli::X86_64::R15,
        ];
        let encoding = reg.hw_enc();
        let mapped = match (self.arch, reg.class()) {
            (Arch::X86_64, RegClass::Int) => X86_64_GENERAL.get(usize::from(encoding)).map(|it| it.0),
            (Arch::X86_64, RegClass::Float) => {
                (encoding < 16).then(|| gimli::X86_64::XMM0.0 + u16::from(encoding))
            }
            (Arch::Aarch64, RegClass::Int) => Some(u16::from(encoding & 31)),
            (Arch::Aarch64, RegClass::Float) => Some(64 + u16::from(encoding)),
            (_, RegClass::Vector) => None,
        };
        // alloc: an error
        mapped.ok_or_else(|| format!("a saved register {reg:?}, which has no DWARF number on {:?}", self.arch))
    }

    /// The rows of `code`, from the unwind instructions Cranelift wrote as it emitted it.
    fn rows(self, code: &CompiledCode) -> Result<FrameRows, String> {
        use cranelift_codegen::isa::unwind::UnwindInst;
        // alloc: an error
        let overflow = || "a frame larger than 4 GiB".to_owned();
        let len = u32::try_from(code.buffer.data().len()).map_err(|_| overflow())?;
        let insts = &code.buffer.unwind_info;
        // alloc: the function's frame rules, kept with its code; most instructions are a row
        let mut rows = Vec::with_capacity(insts.len() + 2);
        let mut clobbers_to_cfa = 0u32;
        let signed = |offset: u32| i32::try_from(offset).map_err(|_| overflow());
        for &(at, ref inst) in insts.iter() {
            match *inst {
                UnwindInst::PushFrameRegs { offset_upward_to_caller_sp: up } => {
                    let up = signed(up)?;
                    rows.push((at, FrameRule::CfaOffset(up)));
                    rows.push((at, FrameRule::Offset(self.frame_pointer, -up)));
                    if let Some((link, offset)) = self.link {
                        rows.push((at, FrameRule::Offset(link, -up + offset)));
                    }
                }
                UnwindInst::DefineNewFrame { offset_upward_to_caller_sp: up, offset_downward_to_clobbers: down } => {
                    rows.push((at, FrameRule::CfaRegister(self.frame_pointer)));
                    clobbers_to_cfa = up.checked_add(down).ok_or_else(overflow)?;
                }
                // The frame is found from the frame pointer, which every frame here has.
                UnwindInst::StackAlloc { .. } => {}
                UnwindInst::SaveReg { clobber_offset, reg } => {
                    let offset = signed(clobber_offset)? - signed(clobbers_to_cfa)?;
                    rows.push((at, FrameRule::Offset(self.register(reg)?, offset)));
                }
                UnwindInst::RegStackOffset { reg, .. } => {
                    // alloc: an error
                    return Err(format!(
                        "a register {reg:?} unwound as an offset from the stack, which {:?} \
                         frames have no rule for",
                        self.arch
                    ));
                }
                UnwindInst::Aarch64SetPointerAuth { return_addresses } => {
                    rows.push((at, FrameRule::RaSignState(return_addresses)));
                }
            }
        }
        Ok(FrameRows { len, rows })
    }
}

/// The rows of the call-site table of `code`: one for each call, or one for each of its
/// handlers where it has some.
fn call_entries(code: &CompiledCode) -> impl Iterator<Item = CallEntry> + '_ {
    code.buffer.call_sites().flat_map(|call_site| {
        let start = call_site.ret_addr - 1;
        let handlers = call_site.exception_handlers;
        let plain = handlers.is_empty().then_some(CallEntry { start, landing_pad: 0, catch: false });
        let handled = handlers.iter().filter_map(move |handler| match *handler {
            MachExceptionHandler::Tag(tag, landing_pad) => Some(CallEntry {
                start,
                landing_pad: landing_pad.as_offset(),
                catch: tag.as_u32() == CATCH,
            }),
            _ => None,
        });
        plain.into_iter().chain(handled)
    })
}

impl FunctionUnwind {
    /// The unwind information of code that came with its own.
    pub(super) fn from_rows(rows: FrameRows, landing_pads: Option<Vec<u8>>) -> Self {
        FunctionUnwind { frame: Frame::Rows(rows), landing_pads }
    }

    /// The unwind information of code of Windows that came with its own: how long the code
    /// is, its `UNWIND_INFO` (x64) or `.xdata` record (ARM64) less the handler, and its calls
    /// that have a landing pad.
    pub(super) fn from_unwind_info(len: u32, info: Vec<u8>, pads: seh::Pads) -> Self {
        let frame = Frame::Windows(seh::Frame { len, info });
        FunctionUnwind { frame, landing_pads: pads.finish() }
    }

    /// The unwind information of stencil code (`blaze_stencil::compile_to`), `None` where it
    /// has none. Where frames are DWARF's, the landing pads are an LSDA of its call sites,
    /// as [`FunctionUnwind::of`] writes Cranelift's; where they are Windows', they are
    /// [`seh::Pads`] of its guarded calls, and x64's `UNWIND_INFO` is final as the stencil
    /// backend gives it.
    pub(super) fn of_stencil(code: &blaze_stencil::CodeRef<'_>) -> Result<Option<Self>, String> {
        use blaze_stencil::code::UnwindRef;
        let Some(unwind) = code.unwind else { return Ok(None) };
        Ok(Some(match unwind {
            UnwindRef::Dwarf { len, rows } => {
                // alloc: the function's frame rules, kept with its code
                let rows = FrameRows { len, rows: rows.to_vec() };
                FunctionUnwind::from_rows(rows, stencil_landing_pads(code)?)
            }
            UnwindRef::Windows { len, info } => {
                // alloc: the function's `UNWIND_INFO`, kept with its code
                FunctionUnwind::from_unwind_info(len, info.to_vec(), stencil_pads(code)?)
            }
            UnwindRef::WindowsArm64 { len, codes } => {
                FunctionUnwind::from_unwind_info(len, arm64_xdata(len, codes)?, stencil_pads(code)?)
            }
        }))
    }

    /// What a v2 build keeps of the function's unwind information: the entry's instructions
    /// at address 0, as the linker places them.
    pub(super) fn v2_parts(&self) -> UnwindParts<'_> {
        let landing_pads = self.landing_pads.as_deref();
        match &self.frame {
            Frame::Windows(frame) => {
                UnwindParts::Windows { len: frame.len, info: &frame.info, landing_pads }
            }
            frame => UnwindParts::Dwarf { fde: Fde(frame), landing_pads },
        }
    }

    /// The function's `.eh_frame` entry, which a frame of Windows has none of.
    fn fde(&self, address: Address) -> Option<FrameDescriptionEntry> {
        self.entry(address).ok()
    }

    /// The function's `.eh_frame` entry, or its frame of Windows.
    fn entry(&self, address: Address) -> Result<FrameDescriptionEntry, &seh::Frame> {
        Ok(match &self.frame {
            Frame::Windows(frame) => return Err(frame),
            Frame::Cranelift(info) => info.to_fde(address),
            Frame::Rows(FrameRows { len, rows }) => {
                let mut fde = FrameDescriptionEntry::new(address, *len);
                for &(offset, rule) in rows {
                    fde.add_instruction(offset, instruction(rule));
                }
                fde
            }
        })
    }
}

/// A function's `.eh_frame` entry, to be written ([`Fde::write`]); never a frame of Windows.
#[derive(Clone, Copy)]
pub(super) struct Fde<'a>(&'a Frame);

impl Fde<'_> {
    /// Appends the entry's call frame instructions and the `DW_CFA_nop`s after them to `out`,
    /// as [`write_fde`] gives them after a CIE of `code_align` and `data_align`: rows of our
    /// own (Cranelift's of x86-64 and AArch64 since [`SystemV`], and stencils') straight from
    /// the rules, with nothing allocated past `out`; Cranelift's own `UnwindInfo` through gimli.
    pub(super) fn write(self, code_align: u32, data_align: i32, out: &mut Vec<u8>) -> Result<(), String> {
        match self.0 {
            Frame::Rows(rows) => write_rows(&rows.rows, code_align, data_align, out),
            Frame::Cranelift(info) => write_fde(info.to_fde(Address::Constant(0)), code_align, data_align, out),
            // alloc: an error
            Frame::Windows(_) => Err("an `.eh_frame` entry of a frame of Windows".to_owned()),
        }
    }
}

/// [`write_fde`] of the FDE of `rows`, written as gimli writes each instruction
/// (`write/cfi.rs`), without its table: the advance to each row's offset in units of
/// `code_align`, the rule with its offset factored by `data_align` where gimli factors it,
/// and `DW_CFA_nop`s to a multiple of 8 bytes, which is where gimli's padding of an FDE with
/// 8-byte addresses and no augmentation ends. `out` is appended to and left as it was on an
/// error.
fn write_rows(
    rows: &[(u32, FrameRule)],
    code_align: u32,
    data_align: i32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let start = out.len();
    let written = write_rows_from(rows, code_align, data_align, out);
    match written {
        Ok(()) => {
            let padding = (8 - (out.len() - start) % 8) % 8;
            out.resize(out.len() + padding, gimli::DW_CFA_nop.0);
            Ok(())
        }
        Err(e) => {
            out.truncate(start);
            Err(e)
        }
    }
}

fn write_rows_from(
    rows: &[(u32, FrameRule)],
    code_align: u32,
    data_align: i32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    use gimli::leb128::write::{signed, unsigned};
    // alloc: an error
    let code_align = u8::try_from(code_align).ok().filter(|it| *it != 0).ok_or_else(|| format!("a code alignment of {code_align}"))?;
    // alloc: an error
    let data_align = i8::try_from(data_align).ok().filter(|it| *it != 0).ok_or_else(|| format!("a data alignment of {data_align}"))?;
    let factored = |offset: i32| -> Result<i32, String> {
        let factor = i32::from(data_align);
        let exact = offset.checked_rem(factor) == Some(0);
        // alloc: an error
        exact.then(|| offset.checked_div(factor)).flatten().ok_or_else(|| format!("a data offset {offset} that is no multiple of {factor}"))
    };
    // Writing to a `Vec` cannot fail.
    let uleb = |out: &mut Vec<u8>, value: u64| {
        let _ = unsigned(out, value);
    };
    let sleb = |out: &mut Vec<u8>, value: i64| {
        let _ = signed(out, value);
    };
    let mut previous = 0u32;
    for &(offset, rule) in rows {
        if offset != previous {
            let delta = offset.checked_sub(previous).filter(|it| it % u32::from(code_align) == 0);
            // alloc: an error
            let delta = delta.ok_or_else(|| format!("a row at {offset} after one at {previous}"))? / u32::from(code_align);
            if delta < 0x40 {
                out.push(gimli::DW_CFA_advance_loc.0 | delta as u8);
            } else if delta < 0x100 {
                out.extend_from_slice(&[gimli::DW_CFA_advance_loc1.0, delta as u8]);
            } else if delta < 0x10000 {
                out.push(gimli::DW_CFA_advance_loc2.0);
                out.extend_from_slice(&(delta as u16).to_le_bytes());
            } else {
                out.push(gimli::DW_CFA_advance_loc4.0);
                out.extend_from_slice(&delta.to_le_bytes());
            }
            previous = offset;
        }
        match rule {
            FrameRule::Cfa(register, offset) if offset < 0 => {
                out.push(gimli::DW_CFA_def_cfa_sf.0);
                uleb(out, register.into());
                sleb(out, factored(offset)?.into());
            }
            FrameRule::Cfa(register, offset) => {
                out.push(gimli::DW_CFA_def_cfa.0);
                uleb(out, register.into());
                uleb(out, offset as u64);
            }
            FrameRule::CfaRegister(register) => {
                out.push(gimli::DW_CFA_def_cfa_register.0);
                uleb(out, register.into());
            }
            FrameRule::CfaOffset(offset) if offset < 0 => {
                out.push(gimli::DW_CFA_def_cfa_offset_sf.0);
                sleb(out, factored(offset)?.into());
            }
            FrameRule::CfaOffset(offset) => {
                out.push(gimli::DW_CFA_def_cfa_offset.0);
                uleb(out, offset as u64);
            }
            FrameRule::Restore(register) if register < 0x40 => {
                out.push(gimli::DW_CFA_restore.0 | register as u8);
            }
            FrameRule::Restore(register) => {
                out.push(gimli::DW_CFA_restore_extended.0);
                uleb(out, register.into());
            }
            FrameRule::Undefined(register) => {
                out.push(gimli::DW_CFA_undefined.0);
                uleb(out, register.into());
            }
            FrameRule::SameValue(register) => {
                out.push(gimli::DW_CFA_same_value.0);
                uleb(out, register.into());
            }
            FrameRule::Offset(register, offset) => match factored(offset)? {
                offset if offset < 0 => {
                    out.push(gimli::DW_CFA_offset_extended_sf.0);
                    uleb(out, register.into());
                    sleb(out, offset.into());
                }
                offset if register < 0x40 => {
                    out.push(gimli::DW_CFA_offset.0 | register as u8);
                    uleb(out, offset as u64);
                }
                offset => {
                    out.push(gimli::DW_CFA_offset_extended.0);
                    uleb(out, register.into());
                    uleb(out, offset as u64);
                }
            },
            FrameRule::RememberState => out.push(gimli::DW_CFA_remember_state.0),
            FrameRule::RestoreState => out.push(gimli::DW_CFA_restore_state.0),
            FrameRule::ArgsSize(size) => {
                out.push(gimli::DW_CFA_GNU_args_size.0);
                uleb(out, size.into());
            }
            // `instruction`'s `ValExpression` of register 34: an expression of one operation.
            FrameRule::RaSignState(on) => {
                let op = if on { gimli::DW_OP_lit1 } else { gimli::DW_OP_lit0 };
                out.extend_from_slice(&[gimli::DW_CFA_val_expression.0, 34, 1, op.0]);
            }
        }
    }
    Ok(())
}

/// Appends to `out` the call frame instructions of `fde` and the `DW_CFA_nop`s after them, as
/// `.eh_frame` has them after a CIE of `code_align` and `data_align` with no augmentation and
/// 8-byte addresses: what a v2 build keeps of a function's entry (`v2/unwind.rs`), whose
/// linker writes the CIE once and the rest of the entry's header. The bytes depend on those
/// two factors alone of the CIE, so the CIE written here has no instructions to copy, and the
/// entry is written into a buffer the thread keeps.
pub(super) fn write_fde(
    fde: FrameDescriptionEntry,
    code_align: u32,
    data_align: i32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    thread_local! {
        // alloc: one buffer a thread, grown to its largest entry and reused
        static WRITTEN: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    // alloc: an error
    let code_align = u8::try_from(code_align).map_err(|_| format!("a code alignment of {code_align}"))?;
    // alloc: an error
    let data_align = i8::try_from(data_align).map_err(|_| format!("a data alignment of {data_align}"))?;
    let encoding = gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
    let cie = CommonInformationEntry::new(encoding, code_align, data_align, gimli::Register(0));
    let mut table = FrameTable::default();
    let id = table.add_cie(cie);
    table.add_fde(id, fde);
    WRITTEN.with_borrow_mut(|written| {
        written.clear();
        // alloc: an error
        table.write_eh_frame(&mut EhFrame(Appended(written))).map_err(|e| e.to_string())?;
        let instructions = fde_instructions(written)?;
        out.extend_from_slice(&written[instructions]);
        Ok(())
    })
}

/// Where the instructions of the FDE after the CIE in `written` are: after its length, its
/// CIE pointer, and its 8-byte address and length.
fn fde_instructions(written: &[u8]) -> Result<std::ops::Range<usize>, String> {
    let mut read = Reader::new(written);
    eh_entry(&mut read)?;
    let mut fde = eh_entry(&mut read)?;
    let not_one = || "not one CIE and one FDE".to_owned(); // alloc: an error
    fde.skip(4 + 16).map_err(|_| not_one())?;
    if !read.is_done() {
        return Err(not_one());
    }
    let end = read.at();
    Ok(end - fde.left()..end)
}

/// The body of the `.eh_frame` entry whose 32-bit length is next.
pub(crate) fn eh_entry<'a>(at: &mut Reader<'a>) -> Result<Reader<'a>, String> {
    let length = at.u32()?;
    if length == u32::MAX {
        return Err("a 64-bit entry".to_owned()); // alloc: the error's message
    }
    Ok(Reader::new(at.take(length as usize)?))
}

/// A gimli writer that appends to a vector it borrows, little-endian as every target v2
/// writes `.eh_frame` for is.
struct Appended<'a>(&'a mut Vec<u8>);

impl Writer for Appended<'_> {
    type Endian = gimli::LittleEndian;

    fn endian(&self) -> Self::Endian {
        gimli::LittleEndian
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn write(&mut self, bytes: &[u8]) -> WriteResult<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> WriteResult<()> {
        let to = self.0.get_mut(offset..).ok_or(gimli::write::Error::OffsetOutOfBounds)?;
        let to = to.get_mut(..bytes.len()).ok_or(gimli::write::Error::LengthOutOfBounds)?;
        to.copy_from_slice(bytes);
        Ok(())
    }
}

/// The LSDA of stencil code's call sites, as [`call_entries`] gives Cranelift's: one row for
/// each call, or one for each of its handlers where it has some. `None` where no call lands
/// anywhere.
fn stencil_landing_pads(code: &blaze_stencil::CodeRef<'_>) -> Result<Option<Vec<u8>>, String> {
    use blaze_stencil::code::Handler;
    let mut lands = false;
    for handler in code.handlers {
        let Handler::Tag { .. } = handler else {
            // alloc: an error
            return Err(format!("copy-and-patch code has a handler of every exception: {handler:?}"));
        };
        lands = true;
    }
    if !lands {
        return Ok(None);
    }
    let handlers = |call: &blaze_stencil::code::CallSite| code.handlers.get(call.handlers.start as usize..call.handlers.end as usize);
    if let Some(call) = code.calls.iter().find(|it| it.ret == 0 || handlers(it).is_none()) {
        // alloc: an error
        return Err(format!("copy-and-patch code has a call site out of its tables: {call:?}"));
    }
    Ok(Some(lsda::write(|| {
        code.calls.iter().flat_map(move |call| {
            let start = call.ret - 1;
            let plain = call.handlers.is_empty().then_some(CallEntry { start, landing_pad: 0, catch: false });
            let handled = handlers(call).unwrap_or_default().iter().filter_map(move |handler| match *handler {
                Handler::Tag { tag, landing } => Some(CallEntry { start, landing_pad: landing, catch: tag == CATCH }),
                Handler::Default { .. } => None,
            });
            plain.into_iter().chain(handled)
        })
    })))
}

/// The landing pads of Windows stencil code: each call the IR builder guarded (by its
/// source location, [`seh::is_guarded`]) returns and looks at the exception, as
/// [`seh::function`] finds Cranelift's. A call that names its handlers is the builder's
/// only where the exception is in a register, which Windows has not.
fn stencil_pads(code: &blaze_stencil::CodeRef<'_>) -> Result<seh::Pads, String> {
    let mut pads = seh::Pads::default();
    for call in code.calls {
        if call.ret == 0 {
            // alloc: an error
            return Err(format!("copy-and-patch code has a call site out of its code: {call:?}"));
        }
        if !call.handlers.is_empty() {
            // alloc: an error
            return Err(format!(
                "copy-and-patch code of Windows has a call returning to {:#x} with handlers, \
                 which its unwinding has no pad for",
                call.ret
            ));
        }
        // The call ends where it returns to, which may be where the next thing starts.
        let after = code.lines.partition_point(|(at, _)| *at <= call.ret - 1);
        if after.checked_sub(1).is_some_and(|at| seh::is_guarded(code.lines[at].1)) {
            pads.returning(call.ret);
        }
    }
    Ok(pads)
}

/// The `E` bit of an ARM64 `.xdata` header: one epilog, whose codes are the prolog's from
/// index 0 (in the epilog field).
const XDATA_E: u32 = 1 << 21;
/// The function length field of an ARM64 `.xdata` header, in 4-byte words.
const XDATA_WORDS: u32 = 1 << 18;

/// The `.xdata` record of ARM64 code `len` bytes long whose unwind codes are `codes` (as
/// Cranelift's `winarm64::UnwindInfo::emit` writes them): the header, then the codes. One
/// epilog, the prolog's codes reversed (`E`), and no handler (`X`): that is added where the
/// record is placed. Code words past the header's 31 take the extension word.
fn arm64_xdata(len: u32, codes: &[u8]) -> Result<Vec<u8>, String> {
    if len % 4 != 0 || len / 4 >= XDATA_WORDS {
        // alloc: an error
        return Err(format!(
            "ARM64 code of {len} bytes: one `.xdata` record covers whole instructions up to 1 MiB"
        ));
    }
    let words = codes.len() / 4;
    if codes.len() % 4 != 0 || words == 0 || words > 255 {
        // alloc: an error
        return Err(format!(
            "{} bytes of ARM64 unwind codes: a `.xdata` record has 1 to 255 words of them",
            codes.len()
        ));
    }
    let mut info = Vec::with_capacity(8 + codes.len());
    if words < 32 {
        info.extend_from_slice(&(len / 4 | XDATA_E | (words as u32) << 27).to_le_bytes());
    } else {
        // Epilog count (here the start index, 0) and code words both zero: the extension
        // word has them.
        info.extend_from_slice(&(len / 4 | XDATA_E).to_le_bytes());
        info.extend_from_slice(&((words as u32) << 16).to_le_bytes());
    }
    info.extend_from_slice(codes);
    Ok(info)
}

/// The symbol of the function the unwinder asks about a frame with landing pads.
pub(super) const PERSONALITY: &str = "rust_eh_personality";

/// The function the unwinder asks about a frame with landing pads: the program's own of that
/// name when it defines one, the runtime's otherwise.
fn personality(module: &mut ObjectModule) -> Result<FuncId, String> {
    const NAME: &str = PERSONALITY;
    if let Some(cranelift_module::FuncOrDataId::Func(own)) = module.declarations().get_name(NAME) {
        return Ok(own);
    }
    let pointer = module.target_config().pointer_type();
    let signature = Signature {
        // alloc: once per object, declaring the personality
        params: vec![
            AbiParam::new(types::I32),
            AbiParam::new(types::I32),
            AbiParam::new(types::I64),
            AbiParam::new(pointer),
            AbiParam::new(pointer),
        ],
        // alloc: once per object, declaring the personality
        returns: vec![AbiParam::new(types::I32)],
        call_conv: module.target_config().default_call_conv,
    };
    // alloc: the error's message
    module.declare_function(NAME, Linkage::Import, &signature).map_err(|e| e.to_string())
}

struct Reloc {
    offset: u32,
    size: u8,
    symbol: usize,
    addend: i64,
    kind: RelocationKind,
    /// Whether what is wanted is the address of a pointer to the symbol.
    indirect: bool,
}
