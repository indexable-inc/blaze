//! The shape of `crates/hir-def/src/deps/image.rs:207` (agents/annoyances/
//! image-part-array-positional-callers.json): an array indexed by an enum's discriminant.

#[derive(Clone, Copy)]
pub enum Part {
    LangItems = 0,
    Signatures = 1,
    DiagnosticItems = 2,
}

impl Part {
    pub const ALL: [Part; 3] = [Part::LangItems, Part::Signatures, Part::DiagnosticItems];
}

pub struct ImageRef<'a> {
    parts: [&'a [u8]; Part::ALL.len()],
    sizes: Vec<usize>,
}

impl<'a> ImageRef<'a> {
    // Found: a fixed array indexed by a `Part` cast to `usize`.
    pub fn part(&self, part: Part) -> &'a [u8] {
        self.parts[part as usize]
    }

    // Found: the same through a path to one variant.
    pub fn signatures(&self) -> &'a [u8] {
        self.parts[Part::Signatures as usize]
    }

    // Not found: an integer cast to `usize` is a position already.
    pub fn nth(&self, n: u32) -> &'a [u8] {
        self.parts[n as usize]
    }

    // Not found: a byte, a `bool` and a `char` are no enum.
    pub fn others(&self, b: u8, yes: bool, c: char) -> usize {
        self.sizes[b as usize] + self.sizes[yes as usize] + self.sizes[c as usize]
    }

    // Not found: a raw pointer cast to `usize` is an address, no enum.
    pub fn at(&self, p: *const u8) -> usize {
        self.sizes[p as usize]
    }
}
