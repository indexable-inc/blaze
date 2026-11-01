//! The call table of a compiled macro crate (macros-tier.md, "Calls"): every function whose
//! address the macro takes, its own or a shim's, by index, with its signature.
//!
//! A function pointer of the confined code is the byte offset of its entry, `index * 16`:
//! the value `fn` items reify to, vtables hold and `call_indirect` takes. A call through
//! one checks, in the code, that the value is a multiple of 16 below the table's size and
//! that the entry's signature is the call's, then calls the entry's code: a pointer that
//! is anything else traps, so an indirect call reaches only the macro's own functions and
//! the shims, each with the arguments it was compiled for. Index 0 is null and matches no
//! signature. The table is the image's, shared by every region running it, and never in a
//! region: the macro can read its pointers but not the table.
//!
//! The shims and the server check a pointer the same way ([`Table::target`]) before they
//! call back into the macro (the bridge buffer's `reserve` and `drop`, a client's `run`).

use blaze_hash::HashMap;

/// One entry, as the confined code reads it: 16 bytes, so a pointer is the entry's offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C, align(16))]
pub(crate) struct Entry {
    /// The code's host address.
    pub(crate) code: u64,
    pub(crate) sig: Sig,
}

const _: () = assert!(size_of::<Entry>() == ENTRY as usize);

/// Bytes of an entry, and so the step between function pointers.
pub(crate) const ENTRY: u64 = 16;
/// The offset of [`Entry::sig`], which the confined code compares.
pub(crate) const SIG: i32 = 8;

/// A signature, interned per image: equal ids are equal signatures.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub(crate) struct Sig(pub(crate) u32);

impl Sig {
    /// The null entry's: no call has it.
    pub(crate) const NONE: Sig = Sig(u32::MAX);
}

/// The image's signatures: each one's encoding (the confining pass's: parameter and return
/// types, purposes and extensions, and the calling convention) to its id.
#[derive(Default)]
pub(crate) struct Signatures {
    ids: HashMap<Box<[u8]>, Sig>,
}

impl Signatures {
    pub(crate) fn intern(&mut self, encoded: &[u8]) -> Sig {
        if let Some(&sig) = self.ids.get(encoded) {
            return sig;
        }
        let sig = Sig(self.ids.len() as u32);
        // alloc: once per signature of an image, when it is compiled
        self.ids.insert(encoded.into(), sig);
        sig
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Why a pointer is no function of the table, said as the expansion's error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Miss {
    Null,
    /// Not a multiple of [`ENTRY`] below the table's size.
    NotAFunction(u64),
    /// A function of another signature.
    Signature { pointer: u64, wanted: Sig, found: Sig },
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Miss::Null => f.write_str("called a null function pointer"),
            Miss::NotAFunction(pointer) => write!(f, "called {pointer:#x}, which is not a function"),
            Miss::Signature { pointer, .. } => {
                write!(f, "called the function at {pointer:#x} through a pointer of another signature")
            }
        }
    }
}

/// The table of one image. Its entries never move once the image is loaded: the confined
/// code holds their address.
pub(crate) struct Table {
    entries: Vec<Entry>,
}

impl Table {
    pub(crate) fn new() -> Table {
        // alloc: once per image
        Table { entries: vec![Entry { code: 0, sig: Sig::NONE }] }
    }

    /// Room for `functions` entries past null, so pushing them never moves the table.
    pub(crate) fn with_capacity(functions: usize) -> Table {
        // alloc: once per image
        let mut entries = Vec::with_capacity(functions + 1);
        entries.push(Entry { code: 0, sig: Sig::NONE });
        Table { entries }
    }

    /// Adds a function, giving its pointer; `None` when the table is full, since growing it
    /// would move it under code that holds its address.
    pub(crate) fn push(&mut self, code: u64, sig: Sig) -> Option<u64> {
        if self.entries.len() == self.entries.capacity() {
            return None;
        }
        let pointer = self.entries.len() as u64 * ENTRY;
        self.entries.push(Entry { code, sig });
        Some(pointer)
    }

    /// The table's address, which the confined code's indirect calls are relocated to.
    pub(crate) fn address(&self) -> u64 {
        self.entries.as_ptr() as u64
    }

    /// Bytes of the table: a pointer is below it.
    pub(crate) fn size(&self) -> u64 {
        self.entries.len() as u64 * ENTRY
    }

    /// The code `pointer` calls with signature `sig`: the check each indirect call makes.
    pub(crate) fn target(&self, pointer: u64, sig: Sig) -> Result<u64, Miss> {
        if pointer == 0 {
            return Err(Miss::Null);
        }
        if !pointer.is_multiple_of(ENTRY) || pointer >= self.size() {
            return Err(Miss::NotAFunction(pointer));
        }
        let entry = self.entries[(pointer / ENTRY) as usize];
        if entry.sig != sig {
            return Err(Miss::Signature { pointer, wanted: sig, found: entry.sig });
        }
        Ok(entry.code)
    }

    pub(crate) fn entry(&self, pointer: u64) -> Option<&Entry> {
        pointer.is_multiple_of(ENTRY).then(|| self.entries.get((pointer / ENTRY) as usize)).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_is_its_entry_and_every_other_value_misses() {
        let mut sigs = Signatures::default();
        let unary = sigs.intern(b"(i64)->i64");
        let binary = sigs.intern(b"(i64,i64)->i64");
        assert_eq!(sigs.intern(b"(i64)->i64"), unary);
        assert_ne!(unary, binary);
        let mut table = Table::with_capacity(2);
        let address = table.address();
        let f = table.push(0x1000, unary).unwrap();
        let g = table.push(0x2000, binary).unwrap();
        assert_eq!(table.address(), address, "pushing within capacity never moves it");
        assert_eq!((f, g), (16, 32));
        if table.entries.capacity() == table.entries.len() {
            assert_eq!(table.push(0x3000, unary), None, "a full table refuses");
        }
        assert_eq!(table.target(f, unary), Ok(0x1000));
        assert_eq!(table.target(g, binary), Ok(0x2000));
        assert_eq!(table.target(0, unary), Err(Miss::Null));
        assert_eq!(table.target(f + 8, unary), Err(Miss::NotAFunction(f + 8)));
        assert_eq!(table.target(48, unary), Err(Miss::NotAFunction(48)));
        assert_eq!(table.target(u64::MAX - 15, unary), Err(Miss::NotAFunction(u64::MAX - 15)));
        assert_eq!(
            table.target(f, binary),
            Err(Miss::Signature { pointer: f, wanted: binary, found: unary })
        );
        assert_eq!(table.entry(g).map(|it| it.code), Some(0x2000));
        assert_eq!(table.entry(g + 1), None);
    }

    #[test]
    fn the_null_entry_matches_no_signature() {
        let table = Table::new();
        assert_eq!(table.size(), ENTRY);
        assert!(table.target(0, Sig(0)).is_err());
        assert_eq!(table.entry(0).map(|it| it.sig), Some(Sig::NONE));
    }

    #[test]
    fn the_entry_is_laid_out_as_the_confined_code_reads_it() {
        assert_eq!(std::mem::offset_of!(Entry, code), 0);
        assert_eq!(std::mem::offset_of!(Entry, sig) as i32, SIG);
        assert_eq!(align_of::<Entry>() as u64, ENTRY);
    }
}
