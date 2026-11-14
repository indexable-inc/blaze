//! A placement change: what [`crate::program::Program`] gives for a demand or a delta, in bytes
//! proportional to the edit, and the one order a driver applies it in ([`apply`]).
//!
//! One driver applies it now: the file writer ([`Sink`] over an executable on disk). It grows
//! each section, writes the bytes at each section's file offset, binds the imports, and on arm64
//! macOS hashes again the code-directory page of each page in [`PlacementChange::pages`]. An
//! edit patches the file and the program is run again; nothing is relinked.
//!
//! **A launcher driver, later** (patching a running program and keeping its state, not a goal
//! now). The change is already what it needs to apply (byte ranges by section, sorted, and the
//! pages they touch); it would add: a way to reach new code from code that is running (a slot
//! per function, or the direct call sites repatched with every thread stopped at a safe point),
//! appended code instead of in-place rewrites (never write over code a thread may be in), a
//! static's new initial value not written over its running state, `MAP_JIT` with W^X toggling
//! per thread and an instruction-cache flush per range on arm64, and an order (code, then the
//! fields pointing at it). None of it is built: no slot, stub or slack serves a running process.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use crate::key::Node;

/// A program's sections. Each has its own reserve of address space, at
/// [`crate::program::Machine::base`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Section {
    /// Function code, each followed by its unwind rows.
    Text,
    /// Statics' values and the file names of panic locations.
    Data,
    /// Panic location records, 24 bytes each: the file name's address (8), its length (8), the
    /// line (4) and the column (4).
    Locations,
    /// One 8-byte word per extern, bound by the driver (the dynamic loader's, in a file).
    Imports,
}

impl Section {
    pub const ALL: [Section; 4] = [Section::Text, Section::Data, Section::Locations, Section::Imports];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Whether the section holds code (mapped executable, its pages signed on arm64 macOS).
    pub fn is_code(self) -> bool {
        matches!(self, Section::Text)
    }
}

/// Bytes to write at `offset` in `section`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Write {
    pub section: Section,
    pub offset: u64,
    pub bytes: Vec<u8, Global>,
}

/// A location record's line to rewrite: the record at `offset` in [`Section::Locations`] now
/// says `line` (its item moved in its file, its code did not change).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LineWrite {
    pub offset: u64,
    pub line: u32,
}

/// An extern for the driver to bind: its import slot and its name.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Import {
    pub slot: u32,
    pub name: Vec<u8, Global>,
}

/// What a program's image must change, all of it sorted, so a change is a function of the
/// program and the delta.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PlacementChange {
    /// Every byte of the image is in `writes`: the image starts empty (a first build, or a
    /// whole layout after a reserve was outgrown or the file compacted).
    pub whole: bool,
    /// The length of each section after the change, by [`Section::index`].
    pub lengths: [u64; 4],
    pub writes: Vec<Write, Global>,
    pub lines: Vec<LineWrite, Global>,
    pub imports: Vec<Import, Global>,
    /// The pages any of it touched, `(section, page index)`: what a code-signed image (arm64
    /// macOS) hashes again.
    pub pages: Vec<(Section, u64), Global>,
    /// The nodes no root reaches any more; their bytes stay where they are, unreferenced, until
    /// the file is compacted.
    pub retired: Vec<Node, Global>,
}

impl PlacementChange {
    /// The bytes it writes: what "proportional to the edit" is measured by.
    pub fn bytes_written(&self) -> u64 {
        let mut total = 4 * self.lines.len() as u64;
        for write in &self.writes {
            total += write.bytes.len() as u64;
        }
        total
    }
}

/// A driver's image: sections of bytes it can grow and write.
pub trait Sink {
    /// Grows (never shrinks) `section` to `len` bytes, zero-filled.
    fn grow(&mut self, section: Section, len: u64);
    fn write(&mut self, section: Section, offset: u64, bytes: &[u8]);
    /// Binds import `slot` to the extern `name`.
    fn bind(&mut self, slot: u32, name: &[u8]);
    /// Called once the change is applied, with the pages it touched.
    fn touched(&mut self, pages: &[(Section, u64)]);
}

/// Applies `change` to `sink` in the one order: lengths, imports, writes, lines, then the
/// touched pages.
pub fn apply(change: &PlacementChange, sink: &mut impl Sink) {
    for section in Section::ALL {
        sink.grow(section, change.lengths[section.index()]);
    }
    for import in &change.imports {
        sink.bind(import.slot, &import.name);
    }
    for write in &change.writes {
        sink.write(write.section, write.offset, &write.bytes);
    }
    for line in &change.lines {
        sink.write(Section::Locations, line.offset + 16, &line.line.to_le_bytes());
    }
    sink.touched(&change.pages);
}
