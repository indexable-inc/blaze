//! A function's unwind information as v2 keeps it ([`Unwind`]): the instructions of its
//! `.eh_frame` entry, apart from where the function is, and its landing pads as read-only
//! data of its own. The linker writes the entries.

use std::sync::OnceLock;

use blaze_codegen::{Data, Relocs, Target, Unwind};
use blaze_format::cursor::Reader;
use cranelift_codegen::{
    gimli::{
        self, RunTimeEndian,
        write::{
            Address, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry,
            FrameTable,
        },
    },
    isa::TargetIsa,
};

use crate::unwind::{FunctionUnwind, UnwindParts, eh_entry as entry};

/// `unwind` as v2 keeps it, with the data of its landing pads, if it has any. `personality`
/// is the target of the routine that reads them.
pub(crate) fn convert(
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    isa: &dyn TargetIsa,
    unwind: &FunctionUnwind,
    personality: impl FnOnce() -> Result<Target, String>,
) -> Result<(Unwind, Option<Data>), String> {
    match unwind.v2_parts() {
        UnwindParts::Dwarf { fde, landing_pads } => {
            let common = common(isa)?;
            let fde = common.fde(fde)?;
            // alloc: the landing pads' data, kept with the code; `Data::new` owns its bytes
            let lsda = landing_pads.map(|pads| Data::new(4, pads.to_vec(), Relocs::new()));
            let personality = match lsda {
                Some(_) => Some(personality()?),
                None => None,
            };
            let unwind = Unwind::Dwarf {
                code_align: common.entry.code_align,
                data_align: common.entry.data_align,
                return_address: common.entry.return_address,
                // clone: `Unwind::Dwarf` owns its CIE's few bytes, kept with the code
                cie: common.entry.cie.clone(),
                fde,
                personality,
                lsda: lsda.as_ref().map(|it| it.id),
            };
            Ok((unwind, lsda))
        }
        UnwindParts::Windows { info, landing_pads: None, .. } => {
            // alloc: the function's unwind info, kept with its code; `Unwind::Windows` owns it
            Ok((Unwind::Windows { info: info.to_vec(), handler: None, lsda: None }, None))
        }
        UnwindParts::Windows { landing_pads: Some(_), .. } => Err(
            // alloc: the error's message
            "a Windows function with landing pads: v2 has no SEH handler data yet".to_owned(),
        ),
    }
}

/// The CIE every function of the process's ISA shares, made and read back once: Cranelift's
/// `create_systemv_cie` depends on the ISA alone.
struct Common {
    /// Its fields as read back, with an FDE of no instructions.
    entry: Entry,
}

impl Common {
    /// The instructions of one function's FDE, as they are written after the shared CIE.
    fn fde(&self, fde: crate::unwind::Fde<'_>) -> Result<Vec<u8>, String> {
        // alloc: the function's FDE, kept with its code
        let mut out = Vec::new();
        fde.write(self.entry.code_align, self.entry.data_align, &mut out)?;
        Ok(out)
    }
}

/// [`Common`] of `isa`, which is the process's: the only ISA a v2 unit compiles with
/// (`backend::check_spec`).
// dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
fn common(isa: &dyn TargetIsa) -> Result<&'static Common, String> {
    static COMMON: OnceLock<Result<Common, String>> = OnceLock::new();
    let process = crate::target().map_err(|e| e.to_string())?; // alloc: the error's message
    if !std::ptr::addr_eq(isa, &**process) {
        // alloc: the error's message
        return Err(format!("unwind information for {}, which is not the process's ISA", isa.triple()));
    }
    let common = COMMON.get_or_init(|| {
        let Some(made) = process.create_systemv_cie() else {
            // alloc: the error's message, once per process
            return Err(format!("{} has no DWARF call frame information", process.triple()));
        };
        let entry = Entry::of(made, FrameDescriptionEntry::new(ANYWHERE, 0))?;
        Ok(Common { entry })
    });
    common.as_ref().map_err(Clone::clone)
}

/// The fields of one CIE and FDE pair, as `.eh_frame` encodes them.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    code_align: u32,
    data_align: i32,
    return_address: u16,
    cie: Vec<u8>,
    fde: Vec<u8>,
}

impl Entry {
    /// Written by gimli, which keeps the instructions of both to itself, and read back.
    fn of(
        cie: CommonInformationEntry,
        fde: gimli::write::FrameDescriptionEntry,
    ) -> Result<Entry, String> {
        let mut table = FrameTable::default();
        let id = table.add_cie(cie);
        table.add_fde(id, fde);
        let mut eh_frame = EhFrame(EndianVec::new(RunTimeEndian::Little));
        // alloc: the error's message; once per process, by `common`
        table.write_eh_frame(&mut eh_frame).map_err(|e| e.to_string())?;
        Entry::read(eh_frame.0.slice())
    }

    /// The CIE and then the FDE of `bytes`, with no augmentation and absolute 8-byte
    /// addresses, as [`Entry::of`] writes them.
    fn read(bytes: &[u8]) -> Result<Entry, String> {
        let mut at = Reader::new(bytes);
        let mut cie = entry(&mut at)?;
        if cie.u32()? != 0 {
            return Err("the first entry is no CIE".to_owned()); // alloc: the error's message
        }
        if cie.u8()? != 1 {
            return Err("a CIE of another version than 1".to_owned()); // alloc: the error's message
        }
        if cie.u8()? != 0 {
            return Err("a CIE with an augmentation".to_owned()); // alloc: the error's message
        }
        // alloc: the error's message
        let code_align = u32::try_from(cie.uleb()?).map_err(|e| e.to_string())?;
        // alloc: the error's message
        let data_align = i32::try_from(cie.sleb()?).map_err(|e| e.to_string())?;
        let return_address = u16::from(cie.u8()?);
        let cie = cie.take_rest().to_vec(); // alloc: once per process, by `common`
        let mut fde = entry(&mut at)?;
        fde.u32()?;
        // Where the function starts and how long it is: the linker's to write.
        fde.skip(16)?;
        let fde = fde.take_rest().to_vec(); // alloc: once per process, by `common`
        if !at.is_done() {
            return Err("more than one CIE and one FDE".to_owned()); // alloc: the error's message
        }
        Ok(Entry { code_align, data_align, return_address, cie, fde })
    }
}

/// Where an FDE for [`Entry::of`] starts: the linker writes the real address.
pub(crate) const ANYWHERE: Address = Address::Constant(0);

#[cfg(test)]
mod tests {
    use cranelift_codegen::gimli::{
        Encoding, Format, Register,
        write::{CallFrameInstruction, CommonInformationEntry, FrameDescriptionEntry},
    };

    use super::*;

    fn x86_cie() -> CommonInformationEntry {
        let encoding = Encoding { address_size: 8, format: Format::Dwarf32, version: 1 };
        let mut cie = CommonInformationEntry::new(encoding, 1, -8, Register(16));
        cie.add_instruction(CallFrameInstruction::Cfa(Register(7), 8));
        cie.add_instruction(CallFrameInstruction::Offset(Register(16), -8));
        cie
    }

    #[test]
    fn reads_back_the_fields_and_instructions() {
        let mut fde = FrameDescriptionEntry::new(ANYWHERE, 32);
        fde.add_instruction(1, CallFrameInstruction::CfaOffset(16));
        fde.add_instruction(1, CallFrameInstruction::Offset(Register(6), -16));
        let entry = Entry::of(x86_cie(), fde).expect("an entry");
        assert_eq!((entry.code_align, entry.data_align, entry.return_address), (1, -8, 16));
        // DW_CFA_def_cfa r7 8, DW_CFA_offset r16 1, padded with DW_CFA_nop.
        assert_eq!(&entry.cie[..5], &[0x0c, 7, 8, 0x80 | 16, 1]);
        assert!(entry.cie[5..].iter().all(|&it| it == 0));
        // DW_CFA_advance_loc 1, DW_CFA_def_cfa_offset 16, DW_CFA_offset r6 2.
        assert_eq!(&entry.fde[..5], &[0x41, 0x0e, 16, 0x80 | 6, 2]);
        assert!(entry.fde[5..].iter().all(|&it| it == 0));
    }

    #[test]
    fn an_entry_does_not_depend_on_the_length_of_the_code() {
        let entry = |len| Entry::of(x86_cie(), FrameDescriptionEntry::new(ANYWHERE, len));
        assert_eq!(entry(16).expect("an entry"), entry(4096).expect("an entry"));
    }
}
