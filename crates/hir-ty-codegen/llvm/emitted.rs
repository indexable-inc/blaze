//! The object file LLVM wrote for a function, or for the functions of a unit, read back as a
//! [`Code`] for each.
//!
//! The code of a function is its section. Constants LLVM put in sections of their own (jump
//! tables, float literals) are laid out after it in the same bytes, and what the code says
//! about where they are is filled in here, so that the rest of ra-jit sees one piece of code
//! that refers to nothing but what the function asked for. What the functions of a unit call
//! of each other they call by the names of requests, as they call anything else; constants
//! more than one of them uses are a data object of their own.

use cranelift_codegen::{
    binemit::Reloc,
    gimli::{
        self, BaseAddresses, CieOrFde, DW_EH_PE_omit, DW_EH_PE_uleb128, EhFrame, EndianSlice,
        LittleEndian, Reader, UnwindSection,
    },
    ir::{self, LibCall, Signature},
};
use cranelift_module::{DataId, FuncId, ModuleReloc, ModuleRelocTarget, default_libcall_names};
use object::{
    Architecture, BinaryFormat, Object, ObjectSection, ObjectSymbol, RelocationFlags,
    RelocationTarget, SectionIndex, SectionKind, SymbolSection, elf, macho, pe,
};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{Symbol, Unhandled};
use crate::{
    ConstBlob,
    debuginfo::{LineRow, SlotPlaces, VariablePlace},
    fragment::{Code, Wants},
    unwind::{FrameRows, FrameRule, FunctionUnwind, seh},
};

pub(super) struct Emitted<'data, 'file> {
    file: &'file object::File<'data>,
    /// The section of the function.
    text: SectionIndex,
    /// The sections of every function of the object that is read.
    texts: &'file FxHashSet<SectionIndex>,
    /// Sections the code of more than one of them refers to.
    shared: &'file FxHashSet<SectionIndex>,
    /// The name of the function, which is that of the file its lines are of.
    name: &'file str,
    /// What became of each section the code leads to.
    placed: FxHashMap<SectionIndex, Placement>,
    /// The sections that are part of the code, in the order they are in `bytes`.
    inline: Vec<SectionIndex>,
    bytes: Vec<u8>,
    /// How long the function is at the start of `bytes`.
    code: u32,
    align: u64,
    relocs: Vec<ModuleReloc>,
    /// See [`Code::elf_types`].
    elf_types: Vec<(u32, u32)>,
}

#[derive(Clone, Copy)]
enum Placement {
    /// Part of the code, this many bytes in.
    Inline(u64),
    /// A data object of its own: constants that hold addresses, which code cannot.
    Blob(DataId),
}

/// The code of each of `functions`, by its name in the object and its signature. What is wrong
/// with the code of one is that function's; what is wrong with the object is every one's.
pub(super) fn read(
    object: &[u8],
    functions: &[(&str, Signature)],
    recorder: &mut Wants,
) -> Result<Vec<Result<Code, Unhandled>>, Unhandled> {
    let file = object::File::parse(object)?;
    let mut found: Vec<Option<(SectionIndex, u64)>> = vec![None; functions.len()];
    for symbol in file.symbols() {
        let (Ok(name), SymbolSection::Section(section)) = (symbol.name(), symbol.section()) else {
            continue;
        };
        if let Some(at) = functions.iter().position(|it| it.0 == plain(&file, name)) {
            found[at] = Some((section, symbol.address()));
        }
    }
    let texts: FxHashSet<SectionIndex> = found.iter().flatten().map(|it| it.0).collect();
    let mut referrers: FxHashMap<SectionIndex, usize> = FxHashMap::default();
    if texts.len() > 1 {
        for &text in &texts {
            let mut targets = FxHashSet::default();
            for (_, relocation) in file.section_by_index(text)?.relocations() {
                if let Destination::Section(target) = Emitted::destination(&file, &relocation)?
                    && !texts.contains(&target)
                {
                    targets.insert(target);
                }
            }
            for target in targets {
                *referrers.entry(target).or_default() += 1;
            }
        }
    }
    let shared = referrers.into_iter().filter(|it| it.1 > 1).map(|it| it.0).collect();
    let mut codes = Vec::with_capacity(functions.len());
    for (at, (_, signature)) in functions.iter().enumerate() {
        let code = match found[at] {
            None => Err(Unhandled::Shape("an object without the function")),
            Some((_, address)) if address != 0 => {
                Err(Unhandled::Shape("code ahead of the function"))
            }
            Some((text, _)) if found.iter().flatten().filter(|it| it.0 == text).count() > 1 => {
                Err(Unhandled::Shape("functions that share a section"))
            }
            Some((text, _)) => {
                let name = functions[at].0;
                Emitted::code(&file, text, &texts, &shared, name, signature.clone(), recorder)
            }
        };
        codes.push(code);
    }
    Ok(codes)
}

/// Where a relocation leads: to a request or a library function by its name, or into a
/// section of the object. A function of the unit is called by the name of its request, which
/// only such a function has as a definition.
enum Destination<'name> {
    Named(&'name str),
    Section(SectionIndex),
}

impl<'data, 'file> Emitted<'data, 'file> {
    fn code(
        file: &'file object::File<'data>,
        text: SectionIndex,
        texts: &'file FxHashSet<SectionIndex>,
        shared: &'file FxHashSet<SectionIndex>,
        name: &'file str,
        signature: Signature,
        recorder: &mut Wants,
    ) -> Result<Code, Unhandled> {
        let mut emitted = Emitted {
            file,
            text,
            texts,
            shared,
            name,
            placed: FxHashMap::default(),
            inline: Vec::new(),
            bytes: Vec::new(),
            code: 0,
            align: 1,
            relocs: Vec::new(),
            elf_types: Vec::new(),
        };
        let unwind = match (emitted.file.format(), emitted.file.architecture()) {
            (BinaryFormat::MachO, _) => {
                emitted.place_paged(text, recorder)?;
                emitted.relocate_paged(recorder)?;
                emitted.unwind_macho()?
            }
            (BinaryFormat::Elf, Architecture::Aarch64) => {
                emitted.place_paged(text, recorder)?;
                emitted.relocate_paged(recorder)?;
                emitted.unwind()?
            }
            (BinaryFormat::Coff, _) => {
                emitted.place(text, recorder)?;
                emitted.relocate(recorder)?;
                emitted.unwind_coff()?
            }
            _ => {
                emitted.place(text, recorder)?;
                emitted.relocate(recorder)?;
                emitted.unwind()?
            }
        };
        let lines = emitted.lines()?;
        let slots = emitted.slots().unwrap_or_default();
        let Emitted { bytes, align, relocs, elf_types, .. } = emitted;
        Ok(Code { signature, bytes, align, relocs, elf_types, unwind, lines, slots })
    }

    /// Where LLVM kept the stack slots `lower` told it of as variables, which are named by
    /// the numbers of the slots. `None` where what it wrote is not understood; a slot it
    /// kept nowhere a debugger can read has no place.
    fn slots(&self) -> Option<SlotPlaces> {
        self.file.section_by_name(".debug_info")?;
        let owned = gimli::Dwarf::load(|id| self.relocated(id.name()).ok_or(())).ok()?;
        let dwarf = owned.borrow(|bytes| EndianSlice::new(bytes, LittleEndian));
        let unit = dwarf.unit(dwarf.units().next().ok()??).ok()?;
        let mut places = SlotPlaces::default();
        let mut entries = unit.entries();
        // How deep the function's subprogram is, while what is read is in it, and how deep
        // another function LLVM built into it is, whose variables are of the other's slots.
        let (mut within, mut inlined): (Option<isize>, Option<isize>) = (None, None);
        while let Some(entry) = entries.next_dfs().ok()? {
            if within.is_some_and(|depth| entry.depth() <= depth) {
                within = None;
            }
            if inlined.is_some_and(|depth| entry.depth() <= depth) {
                inlined = None;
            }
            match entry.tag() {
                gimli::DW_TAG_inlined_subroutine if inlined.is_none() => {
                    inlined = Some(entry.depth());
                }
                gimli::DW_TAG_subprogram => {
                    let low = entry.attr_value(gimli::DW_AT_low_pc);
                    let low = low.and_then(|it| dwarf.attr_address(&unit, it).ok().flatten());
                    if low.and_then(|it| self.local(it)) != Some(0) {
                        continue;
                    }
                    within = Some(entry.depth());
                    if let Some(base) = entry.attr_value(gimli::DW_AT_frame_base)
                        && let Some(base) = base.exprloc_value()
                    {
                        places.frame_base = base.0.slice().to_vec();
                    }
                }
                gimli::DW_TAG_variable if within.is_some() && inlined.is_none() => {
                    // The function's code apart from another's that it is built into has its
                    // variables by what they are in that other: the code is the same.
                    let name = match entry.attr_value(gimli::DW_AT_abstract_origin) {
                        Some(gimli::AttributeValue::UnitRef(at)) => {
                            unit.entry(at).ok()?.attr_value(gimli::DW_AT_name)?
                        }
                        _ => entry.attr_value(gimli::DW_AT_name)?,
                    };
                    let name = dwarf.attr_string(&unit, name).ok()?;
                    let slot = name.to_string().ok()?.parse().ok()?;
                    let mut ranges = Vec::new();
                    match entry.attr_value(gimli::DW_AT_location) {
                        None => {}
                        Some(gimli::AttributeValue::Exprloc(place)) => {
                            ranges.push((0, self.code, place));
                        }
                        Some(list) => {
                            let mut list = dwarf.attr_locations(&unit, list).ok()??;
                            while let Some(place) = list.next().ok()? {
                                let range = place.range;
                                let (Some(begin), Some(end)) =
                                    (self.local(range.begin), self.local(range.end))
                                else {
                                    continue;
                                };
                                ranges.push((begin, end, place.data));
                            }
                        }
                    }
                    let ranges = ranges
                        .into_iter()
                        .filter(|it| Self::is_of_frame(it.2, unit.encoding()))
                        .map(|(from, to, place)| (from, to, place.0.slice().to_vec()))
                        .collect();
                    places.slots.push((slot, VariablePlace::Ranges(ranges)));
                }
                _ => {}
            }
        }
        (!places.frame_base.is_empty()).then_some(places)
    }

    /// Whether `place` says where a value is by registers, the frame and numbers alone, so
    /// that it means the same wherever the function ends up and in whatever unit it is
    /// written.
    fn is_of_frame(
        place: gimli::Expression<EndianSlice<'_, LittleEndian>>,
        encoding: gimli::Encoding,
    ) -> bool {
        use gimli::Operation;
        let mut operations = place.operations(encoding);
        loop {
            match operations.next() {
                Ok(None) => return true,
                Ok(Some(
                    Operation::Register { .. }
                    | Operation::FrameOffset { .. }
                    | Operation::Piece { .. }
                    | Operation::StackValue
                    | Operation::UnsignedConstant { .. }
                    | Operation::SignedConstant { .. }
                    | Operation::PlusConstant { .. }
                    | Operation::Plus
                    | Operation::Minus
                    | Operation::And
                    | Operation::Or
                    | Operation::Shl
                    | Operation::Shr
                    | Operation::Shra
                    | Operation::ImplicitValue { .. },
                )) => {}
                Ok(Some(Operation::RegisterOffset { base_type, .. })) if base_type.0 == 0 => {}
                Ok(Some(Operation::Deref { base_type, space: false, .. })) if base_type.0 == 0 => {}
                _ => return false,
            }
        }
    }

    /// The bytes of the section `name` with what its relocations say filled in, as far as
    /// that is known before linking: each function starts its section, which is at
    /// [`Self::base`], and every other section is on its own.
    fn relocated(&self, name: &str) -> Option<Vec<u8>> {
        let Some(section) = self.file.section_by_name(name) else { return Some(Vec::new()) };
        let mut bytes = section.data().ok()?.to_vec();
        for (offset, relocation) in section.relocations() {
            if relocation.kind() != object::RelocationKind::Absolute {
                return None;
            }
            let symbol = Self::symbol(self.file, &relocation).ok()?;
            let base = match symbol.section() {
                SymbolSection::Section(at) if self.texts.contains(&at) => Self::base(at),
                _ => 0,
            };
            let value = (base + symbol.address()).wrapping_add_signed(relocation.addend());
            let size = usize::from(relocation.size() / 8);
            let at = bytes.get_mut(offset as usize..)?.get_mut(..size)?;
            at.copy_from_slice(value.to_le_bytes().get(..size)?);
        }
        Some(bytes)
    }

    /// Where a section of code is taken to be by [`Self::relocated`]: each 4 GiB on from the
    /// one before, so that an address in the debug sections tells which function it is in.
    fn base(section: SectionIndex) -> u64 {
        (section.0 as u64 + 1) << 32
    }

    /// How far into the function an address [`Self::relocated`] filled in is, if it is in the
    /// function's section.
    fn local(&self, address: u64) -> Option<u32> {
        u32::try_from(address.checked_sub(Self::base(self.text))?).ok()
    }

    /// Which source location the code is of from each offset on, from the line table: the
    /// lines of it are the source locations `lower` marked the instructions with.
    fn lines(&self) -> Result<Vec<LineRow>, Unhandled> {
        const MALFORMED: Unhandled = Unhandled::Shape("a line table that does not parse");
        if self.file.section_by_name(".debug_line").is_none() {
            return Ok(Vec::new());
        }
        let bytes = self.relocated(".debug_line").ok_or(MALFORMED)?;
        let table = gimli::DebugLine::new(&bytes, LittleEndian);
        let program =
            table.program(gimli::DebugLineOffset(0), 8, None, None).map_err(|_| MALFORMED)?;
        let (program, sequences) = program.sequences().map_err(|_| MALFORMED)?;
        // The rows of code of another function of the object that LLVM built into this one
        // are of that one's statements: the code is taken to be of the call.
        let header = program.header();
        let ours = |file: &gimli::FileEntry<EndianSlice<'_, LittleEndian>>| {
            matches!(file.path_name(), gimli::AttributeValue::String(name)
                if name.slice() == self.name.as_bytes())
        };
        let named = header.file_names().iter().any(ours);
        // Each run of code is of one section: the function's are those that start in it.
        let mut ranges = Vec::new();
        for sequence in sequences.iter().filter(|it| self.local(it.start).is_some()) {
            let mut rows = program.resume_from(sequence);
            while let Some((header, row)) = rows.next_row().map_err(|_| MALFORMED)? {
                let line = row.line().map_or(0, |line| line.get());
                if let Some(at) = self.local(row.address())
                    && !row.end_sequence()
                    && line != 0
                    && at < self.code
                    && (!named || row.file(header).is_some_and(ours))
                {
                    ranges.push((at, ir::SourceLoc::new(line as u32)));
                }
            }
        }
        Ok(LineRow::of_ranges(ranges))
    }

    /// Fills the function, which is all of `code`, up to the length of the shortest one
    /// there may be, and answers how long it is then: what LLVM makes of an empty body is a
    /// `ret` alone.
    fn lengthen(code: &mut Vec<u8>) -> Result<u32, Unhandled> {
        let target = crate::Target::selected()
            .map_err(|_| Unhandled::Shape("a target that is not known"))?;
        if let Some((shortest, filler)) = target.arch.shortest_function()
            && code.len() < shortest
        {
            code.resize(shortest, filler);
        }
        Ok(code.len() as u32)
    }

    /// Lays out `first` and every section its relocations lead to.
    fn place(&mut self, first: SectionIndex, recorder: &mut Wants) -> Result<(), Unhandled> {
        let mut pending = vec![first];
        while let Some(index) = pending.pop() {
            if self.placed.contains_key(&index) {
                continue;
            }
            if index != first && self.texts.contains(&index) {
                return Err(Unhandled::Shape("code that refers into another function"));
            }
            let section = self.file.section_by_index(index)?;
            let addresses =
                section.relocations().any(|(_, relocation)| Self::is_address(&relocation));
            // What more than one function uses is had once, as a data object.
            if addresses || self.shared.contains(&index) {
                let blob = match addresses {
                    true => self.blob(&section)?,
                    false => self.blob_elf(&section)?,
                };
                let id = recorder
                    .define_const(blob)
                    .map_err(|_| Unhandled::Shape("constants that cannot be defined"))?;
                self.placed.insert(index, Placement::Blob(id));
                continue;
            }
            match section.kind() {
                SectionKind::Text
                | SectionKind::ReadOnlyData
                | SectionKind::ReadOnlyString
                | SectionKind::ReadOnlyDataWithRel => {}
                _ => return Err(Unhandled::Shape("code that refers to a writable section")),
            }
            let align = section.align().max(1);
            self.align = self.align.max(align);
            let start = (self.bytes.len() as u64).next_multiple_of(align);
            self.bytes.resize(start as usize, 0);
            self.bytes.extend_from_slice(section.data()?);
            if index == first {
                self.code = Self::lengthen(&mut self.bytes)?;
            }
            self.placed.insert(index, Placement::Inline(start));
            self.inline.push(index);
            for (_, relocation) in section.relocations() {
                if let Destination::Section(target) = Self::destination(self.file, &relocation)? {
                    pending.push(target);
                }
            }
        }
        Ok(())
    }

    /// A section of addresses of what the function asked for, such as the table LLVM makes
    /// of a `match` that picks one of several constants.
    fn blob(&self, section: &object::Section<'data, '_>) -> Result<ConstBlob, Unhandled> {
        // Written to only by the loader, which fills in the addresses. A PE image has them
        // with the rest of what is only read.
        let read_only = match self.file.format() {
            BinaryFormat::Coff => section.kind() == SectionKind::ReadOnlyData,
            _ => section.name()?.starts_with(".data.rel.ro"),
        };
        if !read_only {
            return Err(Unhandled::Shape("code that refers to a writable section"));
        }
        let mut blob = ConstBlob {
            bytes: section.data()?.to_vec(),
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: section.align().max(1),
        };
        for (offset, relocation) in section.relocations() {
            let target = match Self::destination(self.file, &relocation)? {
                Destination::Named(name) => Symbol::parse(name),
                Destination::Section(_) => None,
            };
            let absolute = Self::is_address(&relocation);
            // COFF keeps what is added to the address where the address goes.
            let held = match blob.bytes.get_mut(offset as usize..).and_then(<[u8]>::first_chunk_mut)
            {
                Some(field) if absolute && relocation.has_implicit_addend() => {
                    i64::from_le_bytes(std::mem::take(field))
                }
                _ => 0,
            };
            match (target, relocation.addend() + held) {
                (Some(Symbol::Data(index)), addend) if absolute => {
                    blob.relocs.push((offset as u32, DataId::from_u32(index), addend));
                }
                (Some(Symbol::Func(index)), 0) if absolute => {
                    blob.fn_relocs.push((offset as u32, FuncId::from_u32(index)));
                }
                _ => return Err(Unhandled::Shape("constants that hold an address of the code")),
            }
        }
        Ok(blob)
    }

    /// Whether the relocation asks for an address, in the eight bytes one takes.
    fn is_address(relocation: &object::Relocation) -> bool {
        matches!(
            relocation.flags(),
            RelocationFlags::Elf { r_type: elf::R_X86_64_64 | elf::R_AARCH64_ABS64 }
                | RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR64 }
        )
    }

    fn symbol<'f>(
        file: &'f object::File<'data>,
        relocation: &object::Relocation,
    ) -> Result<object::Symbol<'data, 'f>, Unhandled> {
        let RelocationTarget::Symbol(symbol) = relocation.target() else {
            return Err(Unhandled::Shape("a relocation against no symbol"));
        };
        Ok(file.symbol_by_index(symbol)?)
    }

    /// See [`Destination`].
    fn destination(
        file: &'_ object::File<'data>,
        relocation: &object::Relocation,
    ) -> Result<Destination<'data>, Unhandled> {
        let symbol = Self::symbol(file, relocation)?;
        let SymbolSection::Section(section) = symbol.section() else {
            return Ok(Destination::Named(plain(file, symbol.name()?)));
        };
        if symbol.is_global()
            && let name = plain(file, symbol.name()?)
            && Symbol::parse(name).is_some()
        {
            return Ok(Destination::Named(name));
        }
        Ok(Destination::Section(section))
    }

    /// Fills in what points from one part of the code into another, and turns the rest into
    /// relocations against requests.
    fn relocate(&mut self, recorder: &mut Wants) -> Result<(), Unhandled> {
        for &index in &self.inline {
            let section = self.file.section_by_index(index)?;
            let Placement::Inline(base) = self.placed[&index] else { continue };
            for (offset, relocation) in section.relocations() {
                let at = base + offset;
                let (field, addend) = Self::field(&mut self.bytes, at, &relocation)?;
                let symbol = Self::symbol(self.file, &relocation)?;
                let destination = Self::destination(self.file, &relocation)?;
                let within = match destination {
                    Destination::Section(target) => Some(self.placed[&target]),
                    Destination::Named(_) => None,
                };
                if let Some(Placement::Inline(target)) = within {
                    // Both ends are in `bytes`, whose pieces do not move against each other.
                    let (Field::Near | Field::Call) = field else {
                        return Err(Unhandled::Shape("code that holds an address of itself"));
                    };
                    let target = target + symbol.address();
                    let value = target.wrapping_add_signed(addend).wrapping_sub(at);
                    Self::fill(&mut self.bytes, at, value as u32)?;
                    continue;
                }
                let kind = match field {
                    Field::Call => Reloc::X86CallPLTRel4,
                    Field::Near => Reloc::X86PCRel4,
                    Field::Got => Reloc::X86GOTPCRel4,
                    Field::TlsGd => Reloc::ElfX86_64TlsGd,
                    Field::SectionOffset => Reloc::X86SecRel,
                };
                let (name, addend) = match (within, destination) {
                    (Some(Placement::Blob(id)), _) => {
                        (ModuleRelocTarget::user(1, id.as_u32()), addend + symbol.address() as i64)
                    }
                    (_, Destination::Named(name)) => (Self::target(name, recorder)?, addend),
                    (_, Destination::Section(_)) => {
                        return Err(Unhandled::Shape("code that holds an address of itself"));
                    }
                };
                self.relocs.push(ModuleReloc { offset: at as u32, kind, name, addend });
            }
        }
        Ok(())
    }

    /// What the relocation of the four bytes of `bytes` at `at` asks for, and what is added to the
    /// address it names, counted from the field as ELF counts it. COFF keeps that in the
    /// field, which is cleared: whoever applies the relocation is given it apart.
    fn field(
        bytes: &mut [u8],
        at: u64,
        relocation: &object::Relocation,
    ) -> Result<(Field, i64), Unhandled> {
        let field = match relocation.flags() {
            RelocationFlags::Elf { r_type } => match r_type {
                elf::R_X86_64_PLT32 => Field::Call,
                elf::R_X86_64_PC32 => Field::Near,
                // The relaxable forms are loads from the GOT like any other.
                elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
                    Field::Got
                }
                elf::R_X86_64_TLSGD => Field::TlsGd,
                other => return Err(Unhandled::Relocation(other)),
            },
            RelocationFlags::Coff { typ } => match typ {
                // The numbered forms say how many bytes of the instruction follow the
                // field, which `object` has put in the addend.
                pe::IMAGE_REL_AMD64_REL32
                | pe::IMAGE_REL_AMD64_REL32_1
                | pe::IMAGE_REL_AMD64_REL32_2
                | pe::IMAGE_REL_AMD64_REL32_3
                | pe::IMAGE_REL_AMD64_REL32_4
                | pe::IMAGE_REL_AMD64_REL32_5 => Field::Near,
                pe::IMAGE_REL_AMD64_SECREL => Field::SectionOffset,
                other => return Err(Unhandled::Relocation(other.into())),
            },
            _ => return Err(Unhandled::Shape("an object of a format that is not read")),
        };
        if !relocation.has_implicit_addend() {
            return Ok((field, relocation.addend()));
        }
        let held = Self::fill(bytes, at, 0)?;
        Ok((field, relocation.addend() + i64::from(held as i32)))
    }

    /// Puts `value` in the four bytes of `bytes` at `at` and answers what was there.
    fn fill(bytes: &mut [u8], at: u64, value: u32) -> Result<u32, Unhandled> {
        let field = usize::try_from(at)
            .ok()
            .and_then(|at| bytes.get_mut(at..)?.first_chunk_mut())
            .ok_or(Unhandled::Shape("a relocation outside its section"))?;
        Ok(u32::from_le_bytes(std::mem::replace(field, value.to_le_bytes())))
    }

    /// Lays out the function's section. Every other section its relocations lead to is a
    /// data object of its own: AArch64 code finds what is near it by the page it is on,
    /// which only the linker knows.
    fn place_paged(&mut self, text: SectionIndex, recorder: &mut Wants) -> Result<(), Unhandled> {
        let section = self.file.section_by_index(text)?;
        self.align = section.align().max(1);
        self.bytes.extend_from_slice(section.data()?);
        self.code = Self::lengthen(&mut self.bytes)?;
        self.placed.insert(text, Placement::Inline(0));
        self.inline.push(text);
        for (_, relocation) in section.relocations() {
            let Destination::Section(index) = Self::destination(self.file, &relocation)? else {
                continue;
            };
            if self.placed.contains_key(&index) {
                continue;
            }
            if self.texts.contains(&index) {
                return Err(Unhandled::Shape("code that refers into another function"));
            }
            let section = self.file.section_by_index(index)?;
            let blob = match self.file.format() {
                BinaryFormat::MachO => self.blob_macho(&section)?,
                _ => self.blob_elf(&section)?,
            };
            let id = recorder
                .define_const(blob)
                .map_err(|_| Unhandled::Shape("constants that cannot be defined"))?;
            self.placed.insert(index, Placement::Blob(id));
        }
        Ok(())
    }

    /// A section of constants, which may hold addresses of what the function asked for.
    fn blob_elf(&self, section: &object::Section<'data, '_>) -> Result<ConstBlob, Unhandled> {
        if section.relocations().next().is_some() {
            return self.blob(section);
        }
        match section.kind() {
            SectionKind::ReadOnlyData | SectionKind::ReadOnlyString => Ok(ConstBlob {
                bytes: section.data()?.to_vec(),
                relocs: Vec::new(),
                fn_relocs: Vec::new(),
                align: section.align().max(1),
            }),
            _ => Err(Unhandled::Shape("code that refers to a writable section")),
        }
    }

    /// A section of constants, which may hold addresses of what the function asked for.
    fn blob_macho(&self, section: &object::Section<'data, '_>) -> Result<ConstBlob, Unhandled> {
        match section.kind() {
            SectionKind::ReadOnlyData
            | SectionKind::ReadOnlyString
            | SectionKind::ReadOnlyDataWithRel => {}
            // `__DATA,__const`: written to only by the loader, which fills in the addresses.
            SectionKind::Data if section.name()? == "__const" => {}
            _ => return Err(Unhandled::Shape("code that refers to a writable section")),
        }
        let mut blob = ConstBlob {
            bytes: section.data()?.to_vec(),
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: section.align().max(1),
        };
        const HELD: Unhandled = Unhandled::Shape("constants that hold an address of the code");
        for (offset, relocation) in section.relocations() {
            let RelocationFlags::MachO { r_type: macho::ARM64_RELOC_UNSIGNED, r_length: 3, .. } =
                relocation.flags()
            else {
                return Err(HELD);
            };
            let named = match Self::destination(self.file, &relocation)? {
                Destination::Named(name) => Some(name),
                Destination::Section(_) => None,
            };
            // The addend is where the address will be.
            let at = offset as usize;
            let field = blob.bytes.get_mut(at..at + 8).ok_or(HELD)?;
            let addend = i64::from_le_bytes((&*field).try_into().map_err(|_| HELD)?);
            field.fill(0);
            match (named.and_then(Symbol::parse), addend) {
                (Some(Symbol::Data(index)), addend) => {
                    blob.relocs.push((at as u32, DataId::from_u32(index), addend));
                }
                (Some(Symbol::Func(index)), 0) => {
                    blob.fn_relocs.push((at as u32, FuncId::from_u32(index)));
                }
                _ => return Err(HELD),
            }
        }
        Ok(blob)
    }

    /// Turns the relocations of the function into relocations against requests and against
    /// the data objects its constants became.
    fn relocate_paged(&mut self, recorder: &mut Wants) -> Result<(), Unhandled> {
        let section = self.file.section_by_index(self.inline[0])?;
        for (offset, relocation) in section.relocations() {
            let kind = match relocation.flags() {
                RelocationFlags::MachO { r_type, .. } => match r_type {
                    macho::ARM64_RELOC_BRANCH26 => Reloc::Arm64Call,
                    macho::ARM64_RELOC_PAGE21 => Reloc::Aarch64AdrPrelPgHi21,
                    macho::ARM64_RELOC_PAGEOFF12 => Reloc::Aarch64AddAbsLo12Nc,
                    macho::ARM64_RELOC_GOT_LOAD_PAGE21 => Reloc::Aarch64AdrGotPage21,
                    macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12 => Reloc::Aarch64Ld64GotLo12Nc,
                    macho::ARM64_RELOC_TLVP_LOAD_PAGE21 => Reloc::MachOAarch64TlsAdrPage21,
                    macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => Reloc::MachOAarch64TlsAdrPageOff12,
                    other => return Err(Unhandled::Relocation(other.into())),
                },
                RelocationFlags::Elf { r_type } => match r_type {
                    // A jump out of the function is a call that does not come back.
                    elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 => Reloc::Arm64Call,
                    elf::R_AARCH64_ADR_PREL_PG_HI21 => Reloc::Aarch64AdrPrelPgHi21,
                    elf::R_AARCH64_ADD_ABS_LO12_NC => Reloc::Aarch64AddAbsLo12Nc,
                    elf::R_AARCH64_LDST8_ABS_LO12_NC
                    | elf::R_AARCH64_LDST16_ABS_LO12_NC
                    | elf::R_AARCH64_LDST32_ABS_LO12_NC
                    | elf::R_AARCH64_LDST64_ABS_LO12_NC
                    | elf::R_AARCH64_LDST128_ABS_LO12_NC => {
                        // The low bits go in in units of what the load or store moves,
                        // which Cranelift has no kind for: it stands in as an `add`.
                        self.elf_types.push((offset as u32, r_type));
                        Reloc::Aarch64AddAbsLo12Nc
                    }
                    elf::R_AARCH64_ADR_GOT_PAGE => Reloc::Aarch64AdrGotPage21,
                    elf::R_AARCH64_LD64_GOT_LO12_NC => Reloc::Aarch64Ld64GotLo12Nc,
                    elf::R_AARCH64_TLSDESC_ADR_PAGE21 => Reloc::Aarch64TlsDescAdrPage21,
                    elf::R_AARCH64_TLSDESC_LD64_LO12 => Reloc::Aarch64TlsDescLd64Lo12,
                    elf::R_AARCH64_TLSDESC_ADD_LO12 => Reloc::Aarch64TlsDescAddLo12,
                    elf::R_AARCH64_TLSDESC_CALL => Reloc::Aarch64TlsDescCall,
                    other => return Err(Unhandled::Relocation(other)),
                },
                _ => return Err(Unhandled::Shape("an object of a format that is not read")),
            };
            let symbol = Self::symbol(self.file, &relocation)?;
            let (name, addend) = match Self::destination(self.file, &relocation)? {
                Destination::Section(target) => match self.placed[&target] {
                    Placement::Blob(id) => {
                        let within =
                            symbol.address() - self.file.section_by_index(target)?.address();
                        (
                            ModuleRelocTarget::user(1, id.as_u32()),
                            relocation.addend() + within as i64,
                        )
                    }
                    Placement::Inline(_) => {
                        return Err(Unhandled::Shape("code that holds an address of itself"));
                    }
                },
                Destination::Named(name) => (Self::target(name, recorder)?, relocation.addend()),
            };
            self.relocs.push(ModuleReloc { offset: offset as u32, kind, name, addend });
        }
        Ok(())
    }

    /// What an undefined symbol of the object stands for.
    fn target(name: &str, recorder: &mut Wants) -> Result<ModuleRelocTarget, Unhandled> {
        if let Some(symbol) = Symbol::parse(name) {
            return Ok(match symbol {
                Symbol::Func(index) => ModuleRelocTarget::user(0, index),
                Symbol::Data(index) => ModuleRelocTarget::user(1, index),
            });
        }
        let names = default_libcall_names();
        if let Some(libcall) = LibCall::all_libcalls().iter().find(|&&it| names(it) == name) {
            return Ok(ModuleRelocTarget::LibCall(*libcall));
        }
        // A function LLVM calls of its own accord that Cranelift has no name for: a libm
        // rounding function, wide division. Nothing is checked against the signature of an
        // import.
        let id: FuncId = recorder
            .import(name, &recorder.make_signature())
            .map_err(|_| Unhandled::Shape("a library call that cannot be imported"))?;
        Ok(ModuleRelocTarget::user(0, id.as_u32()))
    }

    /// The function's entry of `.eh_frame` as rules, and its entry of `.gcc_except_table`
    /// as it is: the offsets in it are from the start of the function.
    fn unwind(&self) -> Result<Option<FunctionUnwind>, Unhandled> {
        let name = match self.file.format() {
            BinaryFormat::MachO => "__eh_frame",
            _ => ".eh_frame",
        };
        let Some(section) = self.file.section_by_name(name) else { return Ok(None) };
        let eh_frame = EhFrame::new(section.data()?, LittleEndian);
        let bases = BaseAddresses::default().set_eh_frame(0).set_text(0);
        let malformed = |_| Unhandled::Shape("an unwind table that does not parse");
        // Of an object of more than one function, the entries of this one: those whose
        // start, eight bytes in, is relocated against its section.
        let ours: Option<FxHashSet<u64>> = (self.texts.len() > 1).then(|| {
            let starts = section.relocations().filter(|(_, relocation)| {
                Self::symbol(self.file, relocation)
                    .is_ok_and(|it| it.section() == SymbolSection::Section(self.text))
            });
            starts.map(|(offset, _)| offset.wrapping_sub(8)).collect()
        });
        let mut entries = eh_frame.entries(&bases);
        let mut rows = None;
        let mut table = None;
        while let Some(entry) = entries.next().map_err(malformed)? {
            let CieOrFde::Fde(partial) = entry else { continue };
            let fde = partial
                .parse(|_, bases, offset| eh_frame.cie_from_offset(bases, offset))
                .map_err(malformed)?;
            if let Some(ours) = &ours {
                let at = fde.offset() as u64;
                if !ours.contains(&at) {
                    continue;
                }
                table = Some(self.table_of(&section, at..at + fde.entry_len() as u64 + 4)?);
            }
            if rows.is_some() {
                return Err(Unhandled::Shape("more than one unwind table entry"));
            }
            let cie = fde.cie();
            let entry_rules = Self::rules(cie.instructions(&eh_frame, &bases), cie)?;
            let arch = match self.file.architecture() {
                Architecture::Aarch64 => crate::Arch::Aarch64,
                _ => crate::Arch::X86_64,
            };
            if !entry_rules
                .iter()
                .copied()
                .eq(FrameRule::on_entry(arch).iter().map(|&rule| (0, rule)))
            {
                return Err(Unhandled::Shape("an unwind table that starts from other rules"));
            }
            rows = Some(FrameRows {
                len: fde.len() as u32,
                rows: Self::rules(fde.instructions(&eh_frame, &bases), cie)?,
            });
        }
        let Some(rows) = rows else { return Ok(None) };
        let pads = match table {
            Some(table) => table,
            None => self.landing_pads()?,
        };
        Ok(Some(FunctionUnwind::from_rows(rows, pads)))
    }

    /// The table of landing pads the entry of `.eh_frame` at `entry` names, as it is.
    fn table_of(
        &self,
        eh_frame: &object::Section<'data, '_>,
        entry: std::ops::Range<u64>,
    ) -> Result<Option<Vec<u8>>, Unhandled> {
        for (offset, relocation) in eh_frame.relocations() {
            if !entry.contains(&offset) {
                continue;
            }
            let SymbolSection::Section(index) = Self::symbol(self.file, &relocation)?.section()
            else {
                continue;
            };
            let table = self.file.section_by_index(index)?;
            if table.name()?.starts_with(".gcc_except_table") {
                return Self::pads_of(&table).map(Some);
            }
        }
        Ok(None)
    }

    /// The function's entry of `.pdata` as the `UNWIND_INFO` of `.xdata` it names, which is
    /// taken as it is but for the handler: the offsets in it are from the start of the
    /// function.
    fn unwind_coff(&self) -> Result<Option<FunctionUnwind>, Unhandled> {
        const MALFORMED: Unhandled = Unhandled::Shape("an unwind table that does not parse");
        /// The first version of `UNWIND_INFO`, without a flag and with those of a handler
        /// that is called both when a handler is looked for and when the frame is unwound.
        const VERSION: u8 = 1;
        const HANDLED: u8 = VERSION | 0b11 << 3;
        let code = self.code;
        // LLVM writes no entry for a function that calls nothing and leaves the stack as it
        // found it, which the unwinder takes a function without one for. It gets the entry
        // that says as much: whoever looks for where a function ends looks in `.pdata`.
        let Some(pdata) = self.file.section_by_name(".pdata") else {
            let leaf = vec![VERSION, 0, 0, 0];
            return Ok(Some(FunctionUnwind::from_unwind_info(code, leaf, seh::Pads::default())));
        };
        let entry = pdata.data()?;
        // Where the function starts, where it ends and where its `UNWIND_INFO` is.
        let &[_, _, _, _, _, _, _, _, a, b, c, d] = entry else {
            return Err(Unhandled::Shape("more than one unwind table entry"));
        };
        let (_, relocation) =
            pdata.relocations().find(|(offset, _)| *offset == 8).ok_or(MALFORMED)?;
        let symbol = Self::symbol(self.file, &relocation)?;
        let SymbolSection::Section(xdata) = symbol.section() else { return Err(MALFORMED) };
        let xdata = self.file.section_by_index(xdata)?;
        // The field holds what the linker adds the address of the symbol to.
        let start =
            symbol.address() - xdata.address() + u64::from(u32::from_le_bytes([a, b, c, d]));
        let info = xdata.data()?.get(start as usize..).ok_or(MALFORMED)?;
        let &[version_and_flags, _, codes, _, ..] = info else { return Err(MALFORMED) };
        // The codes are two bytes each and an even number of them is there.
        let (frame, rest) = info
            .split_at_checked(4 + usize::from(codes).next_multiple_of(2) * 2)
            .ok_or(MALFORMED)?;
        let pads = match version_and_flags {
            VERSION => seh::Pads::default(),
            // The address of the personality, which is ours to name, and then its table.
            HANDLED => Self::pads_coff(rest.get(4..).ok_or(MALFORMED)?)?,
            // What is left is an entry that goes on from another.
            _ => return Err(Unhandled::Shape("an unwind table in more than one piece")),
        };
        let mut frame = frame.to_vec();
        frame[0] = VERSION;
        Ok(Some(FunctionUnwind::from_unwind_info(code, frame, pads)))
    }

    /// The calls with a landing pad of LLVM's table of them, which is a `.gcc_except_table`
    /// for a personality LLVM does not know.
    fn pads_coff(table: &[u8]) -> Result<seh::Pads, Unhandled> {
        const MALFORMED: Unhandled = Unhandled::Shape("landing pads that do not parse");
        let mut table = EndianSlice::new(table, LittleEndian);
        let mut uleb = |table: &mut EndianSlice<'_, _>| {
            let value = table.read_uleb128().map_err(|_| MALFORMED)?;
            u32::try_from(value).map_err(|_| MALFORMED)
        };
        // Where the pads are counted from, how the types are written and how the calls
        // are: from the start of the function, not at all, and as numbers of any length.
        let header = table.read_u8_array::<[u8; 3]>().map_err(|_| MALFORMED)?;
        if header != [DW_EH_PE_omit.0, DW_EH_PE_omit.0, DW_EH_PE_uleb128.0] {
            return Err(Unhandled::Shape("landing pads that name a type"));
        }
        let len = uleb(&mut table)?;
        let mut calls = table.split(len as usize).map_err(|_| MALFORMED)?;
        let mut pads = seh::Pads::default();
        while !calls.is_empty() {
            let (start, len, pad, _action) =
                (uleb(&mut calls)?, uleb(&mut calls)?, uleb(&mut calls)?, uleb(&mut calls)?);
            if pad != 0 {
                pads.entered(start..start.checked_add(len).ok_or(MALFORMED)?, pad);
            }
        }
        Ok(pads)
    }

    /// The function's table of landing pads as it is: the offsets in it are from the start
    /// of the function.
    fn landing_pads(&self) -> Result<Option<Vec<u8>>, Unhandled> {
        let name = match self.file.format() {
            BinaryFormat::MachO => "__gcc_except_tab",
            _ => ".gcc_except_table",
        };
        self.file.section_by_name(name).map(|table| Self::pads_of(&table)).transpose()
    }

    fn pads_of(table: &object::Section<'data, '_>) -> Result<Vec<u8>, Unhandled> {
        if table.relocations().next().is_some() {
            return Err(Unhandled::Shape("landing pads that name a type"));
        }
        Ok(table.data()?.to_vec())
    }

    /// The function's entry of `__compact_unwind` as the rules it stands for, and its entry
    /// of `__gcc_except_tab` as it is. The compact entry says how the frame is once it is
    /// set up and not where in the function that is, so the rules are given from the first
    /// instruction on: right wherever the function calls, which is where a panic unwinds.
    fn unwind_macho(&self) -> Result<Option<FunctionUnwind>, Unhandled> {
        const MODE: u32 = 0x0f00_0000;
        const FRAMELESS: u32 = 0x0200_0000;
        const FRAME: u32 = 0x0400_0000;
        const STACK: u32 = 0x00ff_f000;
        /// The pairs of registers a function saves at the top of its frame, in the order
        /// they are there, by their DWARF numbers.
        const PAIRS: [(u32, u16); 9] = [
            (0x001, 19),
            (0x002, 21),
            (0x004, 23),
            (0x008, 25),
            (0x010, 27),
            (0x100, 72),
            (0x200, 74),
            (0x400, 76),
            (0x800, 78),
        ];
        let Some(section) = self.file.section_by_name("__compact_unwind") else {
            return Ok(None);
        };
        let data = section.data()?;
        let &[_, _, _, _, _, _, _, _, l0, l1, l2, l3, e0, e1, e2, e3, ..] = data else {
            return Ok(None);
        };
        if data.len() != 32 {
            return Err(Unhandled::Shape("more than one unwind table entry"));
        }
        let (len, encoding) =
            (u32::from_le_bytes([l0, l1, l2, l3]), u32::from_le_bytes([e0, e1, e2, e3]));
        let mut rows = Vec::new();
        // How far below the frame the saved pairs start.
        let mut at = match encoding & MODE {
            FRAMELESS => {
                let size = ((encoding & STACK) >> 12) * 16;
                if size != 0 {
                    rows.push((0, FrameRule::CfaOffset(size as i32)));
                }
                0
            }
            FRAME => {
                rows.extend([
                    (0, FrameRule::Cfa(29, 16)),
                    (0, FrameRule::Offset(30, -8)),
                    (0, FrameRule::Offset(29, -16)),
                ]);
                -16
            }
            // The frame was beyond the compact form, and has an entry of `__eh_frame`.
            _ => return self.unwind(),
        };
        for (pair, register) in PAIRS {
            if encoding & pair != 0 {
                rows.push((0, FrameRule::Offset(register, at - 8)));
                rows.push((0, FrameRule::Offset(register + 1, at - 16)));
                at -= 16;
            }
        }
        Ok(Some(FunctionUnwind::from_rows(FrameRows { len, rows }, self.landing_pads()?)))
    }

    const UNKNOWN_RULE: Unhandled = Unhandled::Shape("an unwind rule there is no answer for");

    fn rules<R: gimli::Reader>(
        mut instructions: gimli::CallFrameInstructionIter<'_, R>,
        cie: &gimli::CommonInformationEntry<R>,
    ) -> Result<Vec<(u32, FrameRule)>, Unhandled> {
        use gimli::CallFrameInstruction as Cfi;
        let scale = cie.data_alignment_factor();
        let factored =
            |offset: i64| offset.checked_mul(scale).and_then(|it| i32::try_from(it).ok());
        let mut at = 0u32;
        let mut rules = Vec::new();
        while let Some(instruction) = instructions.next().map_err(|_| Self::UNKNOWN_RULE)? {
            let rule = match instruction {
                Cfi::AdvanceLoc { delta } => {
                    at += delta * cie.code_alignment_factor() as u32;
                    continue;
                }
                Cfi::Nop => continue,
                Cfi::DefCfa { register, offset } => FrameRule::Cfa(register.0, offset as i32),
                Cfi::DefCfaSf { register, factored_offset } => {
                    FrameRule::Cfa(register.0, factored(factored_offset).ok_or(Self::UNKNOWN_RULE)?)
                }
                Cfi::DefCfaRegister { register } => FrameRule::CfaRegister(register.0),
                Cfi::DefCfaOffset { offset } => FrameRule::CfaOffset(offset as i32),
                Cfi::DefCfaOffsetSf { factored_offset } => {
                    FrameRule::CfaOffset(factored(factored_offset).ok_or(Self::UNKNOWN_RULE)?)
                }
                Cfi::Offset { register, factored_offset } => FrameRule::Offset(
                    register.0,
                    factored(factored_offset as i64).ok_or(Self::UNKNOWN_RULE)?,
                ),
                Cfi::OffsetExtendedSf { register, factored_offset } => FrameRule::Offset(
                    register.0,
                    factored(factored_offset).ok_or(Self::UNKNOWN_RULE)?,
                ),
                Cfi::Restore { register } => FrameRule::Restore(register.0),
                Cfi::SameValue { register } => FrameRule::SameValue(register.0),
                Cfi::Undefined { register } => FrameRule::Undefined(register.0),
                Cfi::RememberState => FrameRule::RememberState,
                Cfi::RestoreState => FrameRule::RestoreState,
                Cfi::ArgsSize { size } => FrameRule::ArgsSize(size as u32),
                _ => return Err(Self::UNKNOWN_RULE),
            };
            rules.push((at, rule));
        }
        Ok(rules)
    }
}

/// What a relocation of x86-64 code asks to be put in four bytes of it.
#[derive(Clone, Copy)]
enum Field {
    /// How far away a function is, or the stub the linker makes to reach it.
    Call,
    /// How far away something is.
    Near,
    /// How far away the entry of the GOT is that holds an address.
    Got,
    /// How far away the pair is that `__tls_get_addr` is called with.
    TlsGd,
    /// How far into its section something is: a thread-local of a PE image.
    SectionOffset,
}

/// The name of a symbol as the module had it: Mach-O puts an underscore before it.
fn plain<'n>(file: &object::File<'_>, name: &'n str) -> &'n str {
    match file.format() {
        BinaryFormat::MachO => name.strip_prefix('_').unwrap_or(name),
        _ => name,
    }
}

impl From<object::Error> for Unhandled {
    fn from(e: object::Error) -> Self {
        Unhandled::Llvm(format!("object: {e}"))
    }
}
