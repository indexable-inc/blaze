//! CodeView for the functions of a COFF object: the `.debug$S` section the linker makes a
//! PDB of, with a procedure symbol for every function and the lines of its code in the C13
//! format. That is what `dbghelp` names a frame and its line from, and what a debugger sets
//! a breakpoint on a line by.
//!
//! There is no `.debug$T`: a procedure has no type, and no variable is told of.

use blaze_format::cursor::Put;
use blaze_hash::HashMap;
use cranelift_module::FuncId;
use cranelift_object::{
    ObjectProduct,
    object::{
        SectionKind,
        pe::{IMAGE_REL_AMD64_SECREL, IMAGE_REL_AMD64_SECTION},
        write::{Relocation, RelocationFlags},
    },
};
use std::fmt::Write;

use super::FunctionDebug;

/// What starts a version string of blaze's, which tells its modules in a PDB from rustc's. No
/// version: the release blaze is is told only to the engine, so that a release changes no crate
/// below it (crates/blaze-engine/build.rs).
const PRODUCER: &str = "blaze";

/// The first four bytes of a `.debug$S` section of the C13 format.
const SIGNATURE_C13: u32 = 4;

/// The kinds of subsection of a `.debug$S` section.
#[repr(u32)]
enum Subsection {
    Symbols = 0xf1,
    Lines = 0xf2,
    StringTable = 0xf3,
    FileChecksums = 0xf4,
}

/// The kinds of symbol record written.
#[repr(u16)]
enum Symbol {
    End = 0x0006,
    ObjName = 0x1101,
    GlobalProc = 0x1110,
    Compile3 = 0x113c,
}

/// `CV_CFL_LANG` of Rust and `CV_CPU_TYPE_e` of x86-64, as rustc's objects have them.
const LANGUAGE_RUST: u32 = 0x15;
const MACHINE_X64: u16 = 0xd0;

/// A file takes this much of the checksums subsection when it has no checksum: where its
/// name is in the string table, and two bytes that say there is none, filled to four.
const FILE_ENTRY: u32 = 8;

struct Function {
    func: FuncId,
    len: u32,
    /// Where its name is in [`CodeView::names`], from and to.
    name: (usize, usize),
    /// Which of [`CodeView::rows`] are its, from and to.
    rows: (usize, usize),
}

/// The module of an object in a PDB, which every function of the object is described in.
#[derive(Default)]
pub(super) struct CodeView {
    directory: String,
    files: Vec<String>,
    file_ids: HashMap<String, u32>,
    functions: Vec<Function>,
    /// The names of the functions, one after another.
    names: String,
    /// The code of the functions from each offset on, one function after another: the file,
    /// as an index into the files of the unit, and the line.
    rows: Vec<(u32, u32, u32)>,
    /// The files of the function being added, as indices into `files`.
    function_files: Vec<u32>,
}

impl CodeView {
    pub(super) fn new(directory: &str) -> Self {
        // alloc: once per object, the directory its files are named from
        CodeView { directory: directory.to_owned(), ..CodeView::default() }
    }

    pub(super) fn add_function(&mut self, func: FuncId, len: u32, debug: &FunctionDebug) {
        if len == 0 {
            return;
        }
        let mut files = std::mem::take(&mut self.function_files);
        files.clear();
        files.extend(debug.files.iter().map(|it| self.file(it)));
        // The code ahead of the first statement sets the frame up, which is of the line the
        // function is declared on, or of the first statement when that line is not known.
        let first = debug.rows.first().map(|it| it.1);
        let lead = debug.declared.or(first).filter(|_| first.is_some_and(|_| debug.rows[0].0 != 0));
        let start = self.rows.len();
        for (offset, place) in
            lead.map(|place| (0, place)).into_iter().chain(debug.rows.iter().copied())
        {
            let Some(&file) = files.get(place.file as usize) else { continue };
            if self.rows[start..].last().is_none_or(|last| (last.1, last.2) != (file, place.line)) {
                self.rows.push((offset, file, place.line));
            }
        }
        self.function_files = files;
        let rows = (start, self.rows.len());
        // `dbghelp` does not read a symbol of rustc's, so a debugger is given what it says.
        let at = self.names.len();
        // Writing to a `String` cannot fail.
        let _ = write!(self.names, "{:#}", rustc_demangle::demangle(&debug.linkage_name));
        self.functions.push(Function { func, len, name: (at, self.names.len()), rows });
    }

    /// The file as the linker is to name it: in full, the way this system writes a path.
    fn file(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.file_ids.get(name) {
            return id;
        }
        let path = std::path::Path::new(&self.directory).join(name);
        // rustc's own sources have a name that is not a path of this machine.
        let full = if name.starts_with("/rustc/") {
            // alloc: once per file of the object, on a miss: the name its checksum entry has
            name.to_owned()
        } else {
            path.to_string_lossy().replace('/', "\\")
        };
        let id = self.files.len() as u32;
        self.files.push(full);
        // alloc: once per file of the object, on a miss: the key it is found by
        self.file_ids.insert(name.to_owned(), id);
        id
    }

    /// Writes the section into the object, when it has a function to describe.
    pub(super) fn emit(self, product: &mut ObjectProduct) -> Result<(), String> {
        if self.functions.is_empty() {
            return Ok(());
        }
        let mut section = Section::new();
        section.subsection(Subsection::Symbols, |out| {
            out.symbol(Symbol::ObjName, |out| {
                out.u32(0);
                out.text("blaze");
            });
            out.symbol(Symbol::Compile3, |out| {
                out.u32(LANGUAGE_RUST);
                out.u16(MACHINE_X64);
                // The versions of the front end and the back end, four numbers each.
                (0..8).for_each(|_| out.u16(0));
                out.text(PRODUCER);
            });
        });
        for function in &self.functions {
            let name = &self.names[function.name.0..function.name.1];
            let rows = &self.rows[function.rows.0..function.rows.1];
            section.subsection(Subsection::Symbols, |out| {
                out.symbol(Symbol::GlobalProc, |out| {
                    // The parent, the end and the next procedure, which the linker fills in.
                    (0..3).for_each(|_| out.u32(0));
                    out.u32(function.len);
                    // Where a debugger stops on entry and before leaving.
                    out.u32(0);
                    out.u32(function.len);
                    // No type.
                    out.u32(0);
                    out.place(function.func);
                    // No flags.
                    out.bytes.push(0);
                    out.text(name);
                });
                out.symbol(Symbol::End, |_| ());
            });
            if rows.is_empty() {
                continue;
            }
            section.subsection(Subsection::Lines, |out| {
                out.place(function.func);
                // No flags: lines have no columns.
                out.u16(0);
                out.u32(function.len);
                for block in rows.chunk_by(|a, b| a.1 == b.1) {
                    out.u32(block[0].1 * FILE_ENTRY);
                    out.u32(block.len() as u32);
                    out.u32(12 + 8 * block.len() as u32);
                    for &(offset, _, line) in block {
                        out.u32(offset);
                        // The line in 24 bits; the bit that says a statement starts here.
                        out.u32(line & 0x00ff_ffff | 0x8000_0000);
                    }
                }
            });
        }
        let size = 1 + self.files.iter().map(|it| it.len() + 1).sum::<usize>();
        let mut names = Vec::with_capacity(size);
        names.push(0u8);
        section.subsection(Subsection::FileChecksums, |out| {
            for file in &self.files {
                out.u32(names.len() as u32);
                // A checksum of no bytes and of no kind, filled to four bytes.
                out.u32(0);
                names.extend_from_slice(file.as_bytes());
                names.push(0);
            }
        });
        section.subsection(Subsection::StringTable, |out| out.bytes.extend_from_slice(&names));

        let Section { out: Record { bytes, places } } = section;
        let object = &mut product.object;
        // alloc: once per object, the section's name, which the object owns
        let at = object.add_section(Vec::new(), b".debug$S".to_vec(), SectionKind::Debug);
        object.section_mut(at).set_data(bytes, 4);
        for (offset, func) in places {
            let symbol = product.function_symbol(func);
            // Where the function is in its section, and then which section that is.
            for (offset, typ) in
                [(offset, IMAGE_REL_AMD64_SECREL), (offset + 4, IMAGE_REL_AMD64_SECTION)]
            {
                let flags = RelocationFlags::Coff { typ };
                product
                    .object
                    .add_relocation(at, Relocation { offset, symbol, flags, addend: 0 })
                    // alloc: the error's message
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

struct Section {
    out: Record,
}

impl Section {
    fn new() -> Self {
        let mut out = Record::default();
        out.u32(SIGNATURE_C13);
        Section { out }
    }

    /// Adds a subsection of what `fill` writes, which starts and ends at a multiple of four.
    fn subsection(&mut self, kind: Subsection, fill: impl FnOnce(&mut Record)) {
        let out = &mut self.out;
        out.u32(kind as u32);
        let len_at = out.bytes.len();
        out.u32(0);
        fill(out);
        let len = (out.bytes.len() - len_at - 4) as u32;
        out.bytes.patch_u32(len_at, len);
        out.align();
    }
}

/// Bytes of the section, and where in them a function is to be named by the linker: four
/// bytes of where it is in its section and then two of which section.
#[derive(Default)]
struct Record {
    bytes: Vec<u8>,
    places: Vec<(u64, FuncId)>,
}

impl Record {
    fn u16(&mut self, value: u16) {
        self.bytes.put_u16(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.put_u32(value);
    }

    fn text(&mut self, text: &str) {
        self.bytes.put_cstr(text.as_bytes());
    }

    fn place(&mut self, func: FuncId) {
        self.places.push((self.bytes.len() as u64, func));
        self.u32(0);
        self.u16(0);
    }

    fn align(&mut self) {
        self.bytes.resize(self.bytes.len().next_multiple_of(4), 0);
    }

    /// Adds a symbol record of what `fill` writes. Its length, which does not count itself,
    /// is a multiple of four less two.
    fn symbol(&mut self, kind: Symbol, fill: impl FnOnce(&mut Record)) {
        let len_at = self.bytes.len();
        self.u16(0);
        self.u16(kind as u16);
        fill(self);
        self.align();
        let len = (self.bytes.len() - len_at - 2) as u16;
        self.bytes[len_at..len_at + 2].copy_from_slice(&len.to_le_bytes());
    }
}
