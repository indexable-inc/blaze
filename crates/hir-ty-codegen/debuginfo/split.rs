//! Split DWARF: what a debugger reads of the code is kept apart from what a linker links.
//! The object keeps the lines and the addresses, with a short unit that says where the rest
//! is (the skeleton). The rest, the entries of the functions, their variables and types, is
//! in a file of its own for each unit (`.debug_info.dwo` and the others), which a debugger
//! reads from there, or from the package the files are put in. One file has one unit, since
//! that is all some readers look for in it.
//!
//! The sections are those of GNU's extension of DWARF 4, which rustc writes too: an entry
//! kept apart names no address, only the place of one in `.debug_addr`.

use blaze_format::cursor::Reader;
use blaze_hash::HashMap;
use cranelift_codegen::gimli::{
    self, DW_AT_GNU_addr_base, DW_AT_GNU_dwo_id, DW_AT_GNU_dwo_name, DW_AT_GNU_ranges_base,
    DW_AT_comp_dir, DW_AT_low_pc, DW_AT_ranges, DW_AT_stmt_list, DW_CHILDREN_no,
    DW_FORM_GNU_addr_index, DW_FORM_addr, DW_FORM_data8, DW_FORM_implicit_const,
    DW_FORM_sec_offset, DW_FORM_string, DW_FORM_udata, DW_TAG_compile_unit, Encoding, SectionId,
    write::{
        Address, DebugLine, LineProgram, LineStringTable, Range, Sections, StringTable, Writer,
    },
};
use cranelift_object::object::{
    self, BinaryFormat, SectionFlags, SectionKind, elf,
    write::{Object, StandardSegment},
};

use super::{Section, SectionWriter, Sidecar, Target};

/// A list entry of `.debug_loc.dwo` that names its two ends by their places in
/// `.debug_addr`, and the one that ends a list.
const DW_LLE_GNU_START_END_ENTRY: u8 = 2;
const DW_LLE_GNU_END_OF_LIST_ENTRY: u8 = 0;

/// The name of the file of the unit that is `nth` of those `object` has kept apart.
pub(super) fn file(object: &str, nth: usize) -> String {
    // alloc: the file's name, kept; once per unit of up to 256 functions
    format!("{object}.{nth}.dwo")
}

/// The message of an error of writing a section or an object.
fn message(error: impl std::fmt::Display) -> String {
    // alloc: the error's message
    error.to_string()
}

/// The shapes of a unit's entries, with the start of an entry's code, which was written as
/// a plain number, said to be the place of an address among those the object keeps. Both
/// are written the same way, so the entries stay as they are.
fn by_index(abbrev: &SectionWriter) -> Result<SectionWriter, String> {
    // alloc: the error's message
    let short = |_| "the shapes of a unit's entries end short".to_owned();
    let mut read = gimli::EndianSlice::new(abbrev.bytes.slice(), gimli::LittleEndian);
    let mut next = || gimli::leb128::read::unsigned(&mut read).map_err(short);
    let mut renamed = SectionWriter::default();
    let mut put = |value| renamed.write_uleb128(value).map_err(message);
    loop {
        let code = next()?;
        put(code)?;
        if code == 0 {
            break;
        }
        // The tag, and whether the entry has children: one byte, written as it is read.
        put(next()?)?;
        put(next()?)?;
        loop {
            let (name, form) = (next()?, next()?);
            let index = name == u64::from(DW_AT_low_pc.0) && form == u64::from(DW_FORM_udata.0);
            put(name)?;
            put(if index { DW_FORM_GNU_addr_index.0.into() } else { form })?;
            if form == u64::from(DW_FORM_implicit_const.0) {
                // alloc: the error's message
                return Err("a shape with a value of its own".to_owned());
            }
            if name == 0 && form == 0 {
                break;
            }
        }
    }
    Ok(renamed)
}

/// A unit whose entries are kept apart.
pub(super) struct Apart {
    pub(super) unit: gimli::write::Unit,
    pub(super) lines: LineProgram,
    /// The code of the unit's functions.
    pub(super) ranges: Vec<Range>,
    /// The addresses its entries name by their place in this list.
    pub(super) addresses: Vec<Address>,
    /// What tells the entries of this unit from those of any other, and from those of
    /// another build of it.
    pub(super) id: u64,
}

/// The sections of an object whose units are kept apart, as they are added to.
pub(super) struct Split {
    encoding: Encoding,
    directory: Vec<u8>,
    /// What the name of each file the entries are read from starts with.
    name: String,
    object: (BinaryFormat, object::Architecture),
    info: SectionWriter,
    abbrev: SectionWriter,
    line: DebugLine<SectionWriter>,
    ranges: SectionWriter,
    addr: SectionWriter,
    /// The location lists of the unit being added.
    loc_apart: SectionWriter,
    /// Of the unit being added, as [`Split::locations`] finds them: the place among the
    /// relocations of its location lists of the one at each offset, the place in
    /// `.debug_addr` of each address they name, and where each list is now by where it was.
    loc_relocs: HashMap<u64, usize>,
    indices: HashMap<(u32, i64), u64>,
    lists: HashMap<i64, usize>,
    files: Vec<Sidecar>,
}

impl Split {
    pub(super) fn new(
        encoding: Encoding,
        directory: Vec<u8>,
        name: String,
        like: &Object<'_>,
    ) -> Self {
        Split {
            encoding,
            directory,
            name,
            object: (like.format(), like.architecture()),
            info: SectionWriter::default(),
            abbrev: SectionWriter::default(),
            line: DebugLine(SectionWriter::default()),
            ranges: SectionWriter::default(),
            addr: SectionWriter::default(),
            loc_apart: SectionWriter::default(),
            loc_relocs: HashMap::default(),
            indices: HashMap::default(),
            lists: HashMap::default(),
            // alloc: once per object, one file pushed for each unit of up to 256 functions
            files: Vec::new(),
        }
    }

    /// Writes one unit: its entries apart, and its lines, addresses and skeleton to link.
    pub(super) fn add(&mut self, apart: Apart) -> Result<(), String> {
        if self.abbrev.len() == 0 {
            self.skeleton_abbrev().map_err(message)?;
        }
        let Apart { unit, lines, ranges, addresses, id } = apart;
        let mut dwarf = gimli::write::Dwarf::new();
        dwarf.units.add(unit);
        let mut written = Sections::new(SectionWriter::default());
        dwarf.write(&mut written).map_err(message)?;
        if written.debug_str.len() != 0 || written.debug_line.len() != 0 {
            // alloc: the error's message
            return Err("a unit kept apart has strings or lines of its own".to_owned());
        }

        // The names of the files are in the lines themselves, so these stay empty.
        let (mut line_strings, mut strings) = (LineStringTable::default(), StringTable::default());
        let lines = lines
            .write(&mut self.line, self.encoding, &mut line_strings, &mut strings)
            .map_err(message)?;

        let size = self.encoding.address_size;
        let of_unit = self.ranges.len();
        for range in ranges {
            let Range::StartLength { begin, length } = range else {
                // alloc: the error's message
                return Err("a function of no one stretch of code".to_owned());
            };
            let end = super::past(begin, length as u32);
            self.ranges.write_address(begin, size).map_err(message)?;
            self.ranges.write_address(end, size).map_err(message)?;
        }
        self.ranges.write_udata(0, size).map_err(message)?;
        self.ranges.write_udata(0, size).map_err(message)?;
        let ranges_base = self.ranges.len();
        self.ranges.append(&written.debug_ranges.0);

        let addr_base = self.addr.len();
        for address in addresses {
            self.addr.write_address(address, size).map_err(message)?;
        }
        self.locations(&written.debug_loc.0, addr_base)?;

        let mut info = written.debug_info.0;
        for reloc in std::mem::take(&mut info.relocs) {
            let value = match reloc.target {
                Target::Section(SectionId::DebugAbbrev) => reloc.addend,
                Target::Section(SectionId::DebugRanges) => reloc.addend,
                Target::Section(SectionId::DebugLoc) => match self.lists.get(&reloc.addend) {
                    Some(&at) => at as i64,
                    // alloc: the error's message
                    None => return Err("a unit kept apart reads a list it has not".to_owned()),
                },
                Target::Section(section) => {
                    // alloc: the error's message
                    return Err(format!("a unit kept apart reads {}", section.name()));
                }
                Target::Function(_) => {
                    // alloc: the error's message
                    return Err("a unit kept apart names an address".to_owned());
                }
            };
            info.write_udata_at(reloc.offset as usize, value as u64, reloc.size).map_err(message)?;
        }
        let name = file(&self.name, self.files.len());
        let lists = std::mem::take(&mut self.loc_apart);
        let bytes = self.file([
            (SectionId::DebugInfo, &info),
            (SectionId::DebugAbbrev, &by_index(&written.debug_abbrev.0)?),
            (SectionId::DebugLoc, &lists),
        ])?;
        self.skeleton(name.as_bytes(), lines.0, id, of_unit, ranges_base, addr_base)
            .map_err(message)?;
        self.files.push(Sidecar { name, bytes });
        Ok(())
    }

    /// The file of one unit's entries: an object of nothing but these sections.
    fn file(&self, sections: [(SectionId, &SectionWriter); 3]) -> Result<Vec<u8>, String> {
        let (format, architecture) = self.object;
        // The sections are written with the low byte first, as every target of ours has it.
        let mut object = Object::new(format, architecture, object::Endianness::Little);
        for (id, written) in sections {
            let bytes = written.bytes.slice();
            let Some(name) = id.dwo_name().filter(|_| !bytes.is_empty()) else { continue };
            // alloc: `object` takes the names owned, of three sections a unit of 256 functions
            let segment = object.segment_name(StandardSegment::Debug).to_vec();
            // alloc: `object` takes the section's name owned
            let at = object.add_section(segment, name.as_bytes().to_vec(), SectionKind::Debug);
            let section = object.section_mut(at);
            section.set_data(bytes, 1);
            section.flags =
                SectionFlags::Elf { sh_type: elf::SHT_PROGBITS, sh_flags: elf::SHF_EXCLUDE.into() };
        }
        object.write().map_err(message)
    }

    /// The location lists of a unit as `.debug_loc.dwo` has them, with the addresses they
    /// name put in `.debug_addr`. Leaves where each list is now, by where it was, in
    /// [`Split::lists`].
    fn locations(&mut self, loc: &SectionWriter, addr_base: usize) -> Result<(), String> {
        let size = usize::from(self.encoding.address_size);
        let bytes = loc.bytes.slice();
        self.loc_relocs.clear();
        self.loc_relocs.extend(loc.relocs.iter().enumerate().map(|(nth, it)| (it.offset, nth)));
        self.indices.clear();
        self.lists.clear();
        let relocs = &self.loc_relocs;
        let reloc = |offset: usize| relocs.get(&(offset as u64)).map(|&nth| &loc.relocs[nth]);
        // alloc: the error's message
        let short = || "a location list ends short".to_owned();
        let mut entries = 0;
        let mut read = Reader::new(bytes);
        let mut begins = true;
        while !read.is_done() {
            let at = read.at();
            if begins {
                self.lists.insert(at as i64, self.loc_apart.len());
                begins = false;
            }
            let ends = read.take(2 * size).map_err(|_| short())?;
            let (begin, end) = (reloc(at), reloc(at + size));
            let (Some(begin), Some(end)) = (begin, end) else {
                if begin.is_some() || end.is_some() || ends.iter().any(|&it| it != 0) {
                    // alloc: the error's message
                    return Err("a location list of no function's addresses".to_owned());
                }
                self.loc_apart.write_u8(DW_LLE_GNU_END_OF_LIST_ENTRY).map_err(message)?;
                begins = true;
                continue;
            };
            self.loc_apart.write_u8(DW_LLE_GNU_START_END_ENTRY).map_err(message)?;
            for end in [begin, end] {
                let Target::Function(func) = end.target else {
                    // alloc: the error's message
                    return Err("a location list of no function's addresses".to_owned());
                };
                let index = match self.indices.get(&(func.as_u32(), end.addend)) {
                    Some(&index) => index,
                    None => {
                        let index = ((self.addr.len() - addr_base) / size) as u64;
                        let address =
                            Address::Symbol { symbol: func.as_u32() as usize, addend: end.addend };
                        self.addr.write_address(address, size as u8).map_err(message)?;
                        self.indices.insert((func.as_u32(), end.addend), index);
                        index
                    }
                };
                self.loc_apart.write_uleb128(index).map_err(message)?;
            }
            let start = read.at();
            let len = read.u16().map_err(|_| short())?;
            read.skip(usize::from(len)).map_err(|_| short())?;
            self.loc_apart.write(&bytes[start..read.at()]).map_err(message)?;
            entries += 1;
        }
        if loc.relocs.len() != 2 * entries {
            // alloc: the error's message
            return Err("a location kept apart names an address".to_owned());
        }
        Ok(())
    }

    /// The one shape every skeleton has.
    fn skeleton_abbrev(&mut self) -> gimli::write::Result<()> {
        let w = &mut self.abbrev;
        w.write_uleb128(1)?;
        w.write_uleb128(DW_TAG_compile_unit.0.into())?;
        w.write_u8(DW_CHILDREN_no.0)?;
        for (name, form) in [
            (DW_AT_stmt_list, DW_FORM_sec_offset),
            (DW_AT_comp_dir, DW_FORM_string),
            (DW_AT_GNU_dwo_name, DW_FORM_string),
            (DW_AT_GNU_dwo_id, DW_FORM_data8),
            (DW_AT_low_pc, DW_FORM_addr),
            (DW_AT_ranges, DW_FORM_sec_offset),
            (DW_AT_GNU_ranges_base, DW_FORM_sec_offset),
            (DW_AT_GNU_addr_base, DW_FORM_sec_offset),
        ] {
            w.write_uleb128(name.0.into())?;
            w.write_uleb128(form.0.into())?;
        }
        // The end of the shape, and of the shapes.
        w.write(&[0, 0, 0])
    }

    /// The unit left in the object for one kept apart: where its lines and addresses are,
    /// and where its entries are read from.
    fn skeleton(
        &mut self,
        name: &[u8],
        lines: usize,
        id: u64,
        ranges: usize,
        ranges_base: usize,
        addr_base: usize,
    ) -> gimli::write::Result<()> {
        let size = self.encoding.address_size;
        let w = &mut self.info;
        let length_at = w.len();
        w.write_u32(0)?;
        w.write_u16(self.encoding.version)?;
        w.write_offset(0, SectionId::DebugAbbrev, 4)?;
        w.write_u8(size)?;
        w.write_uleb128(1)?;
        w.write_offset(lines, SectionId::DebugLine, 4)?;
        for text in [self.directory.as_slice(), name] {
            w.write(text)?;
            w.write_u8(0)?;
        }
        w.write_u64(id)?;
        w.write_udata(0, size)?;
        w.write_offset(ranges, SectionId::DebugRanges, 4)?;
        w.write_offset(ranges_base, SectionId::DebugRanges, 4)?;
        w.write_offset(addr_base, SectionId::DebugAddr, 4)?;
        let length = w.len() - length_at - 4;
        w.write_udata_at(length_at, length as u64, 4)
    }

    /// The sections to link, and the files of what is kept apart.
    pub(super) fn finish(self) -> (Vec<Section>, Vec<Sidecar>) {
        let linked = [
            (SectionId::DebugInfo, self.info),
            (SectionId::DebugAbbrev, self.abbrev),
            (SectionId::DebugLine, self.line.0),
            (SectionId::DebugRanges, self.ranges),
            (SectionId::DebugAddr, self.addr),
        ];
        let linked = linked.into_iter().map(|(id, written)| Section { id, group: None, written });
        // alloc: once per object, the five sections it links
        (linked.collect(), self.files)
    }
}
