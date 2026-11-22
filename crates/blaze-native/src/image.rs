//! An image at image-relative addresses (base 0): what `layout::lay` makes of a resolved
//! [`Plan`](crate::resolve::Plan), and what the writers (W4's Mach-O dylib, W5's ELF shared
//! object, W9's export) only place headers around (NATIVE.md 3.1, 3.4, 3.6).
//!
//! Address 0 is the start of the file and of the mapping: the first `Layout::header_reserve`
//! bytes are the writer's (its header and load commands), and `bytes` starts there. Every
//! region of [`Region`] is listed, in address order, even when empty (`len` 0), so a writer
//! finds each by its name.
//!
//! What a region holds:
//! - `Text`: the members' code in (pull order, section order); then one 12-byte stub per Rust
//!   word or import a branch reaches (`adrp x16, cell@PAGE; ldr x16, [x16, cell@PAGEOFF];
//!   br x16`); then one 12-byte veneer per far branch target (`adrp x16; add x16; br x16`).
//! - `Const`: read-only data with no absolute pointer, C strings, exception tables.
//! - `EhFrame`: the members' call frame information, CIEs deduplicated, FDEs of dropped COMDAT
//!   sections left out.
//! - `UnwindInfo`: on Mach-O `__TEXT,__unwind_info`; on ELF `.eh_frame_hdr` (the writer points
//!   `PT_GNU_EH_FRAME` at it).
//! - `Got` (on a new page: the writable segment starts here): one 8-byte cell per distinct
//!   target a GOT reloc, a branch to an import or a compact unwind personality reaches; on ELF
//!   also a 16-byte pair per TLS descriptor and a cell per initial-exec TLS offset.
//! - `Data` (on a new page after a non-empty `Got`): the descriptor `__blaze_native_v1` first
//!   (BNL2, descriptor.rs: the word, init and pointer counts, their image-relative addresses
//!   and the names table's), then its names table (word order), then its pointer rows (one per
//!   `FixTo::Word` fixup, sorted by cell), then the members' data, and read-only data that
//!   holds an absolute pointer (it needs a fixup).
//! - `Words`: one 8-byte cell per [`Image::words`] name; the launcher fills cell i.
//! - `Inits`: the members' initializer pointers in link order; `Image::inits` of them.
//! - `ThreadVars`, `ThreadData`, `ThreadZero`: Mach-O `__thread_vars`, `__thread_data`,
//!   `__thread_bss`; ELF `.tdata`, `.tbss` (the TLS template is `ThreadData` then
//!   `ThreadZero`).
//! - `Zero`: zero-fill data, then the winning common symbols.

use blaze_own::AVec;

use crate::Platform;
use crate::libraries::Linked;
use crate::unwind::Row;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Region { Text, Const, EhFrame, UnwindInfo, Got, Data, Words, Inits, ThreadVars, ThreadData, ThreadZero, Zero }

pub struct Placed { pub region: Region, pub at: u64, pub len: u64, pub align_log2: u8 }

/// What a 64-bit pointer at a fixup's address becomes at load: `Rebase` the image's base plus
/// `address`, with `high8` in its top byte (a tag the program keeps in the pointer, 0 for nearly
/// every one; dyld's `dyld_chained_ptr_64_rebase.high8`); `Import` the address of import `index`
/// (an index of [`Image::imports`]) plus `addend`; `Word` the function of word `index`. A `Word`
/// cell is the launcher's to write (the descriptor's pointer rows list it): a dylib writer emits
/// no dynamic fixup for it; an executable writer (W9) makes it a direct reference to the Rust
/// function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FixTo { Rebase { address: Addr56, high8: u8 }, Import { index: u32, addend: i64 }, Word { index: u32 } }

/// An image address a rebase names: below 2^56 by construction, so the pointer's top byte is
/// never part of it ([`Addr56::new`] refuses where the value is made; [`FixTo::rebase`] splits a
/// tagged pointer into the two fields, and nothing shifts a packed word).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Addr56(u64);

impl Addr56 {
    /// The bits an address holds: everything below the pointer's top byte.
    pub const MASK: u64 = (1 << 56) - 1;

    /// `address`, or `None` when it reaches into the top byte.
    pub const fn new(address: u64) -> Option<Addr56> {
        if address & !Self::MASK == 0 { Some(Addr56(address)) } else { None }
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The 64-bit pointer the program's bytes hold: `high8` in the top byte over the address.
    pub const fn tagged(self, high8: u8) -> u64 {
        ((high8 as u64) << 56) | self.0
    }
}

impl FixTo {
    /// The rebase of a 64-bit pointer as the program's bytes hold it: its top byte is the tag
    /// (`high8`; arm64 non-unique RTTI's bit 63 on a typeinfo's name), the 56 bits below it the
    /// image address. Total: every pointer splits so; whether the address is an image place is
    /// the layout's to check (layout.rs `Reach::Rebase`).
    pub const fn rebase(pointer: u64) -> FixTo {
        FixTo::Rebase { address: Addr56(pointer & Addr56::MASK), high8: (pointer >> 56) as u8 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixup { pub at: u64, pub to: FixTo } // a 64-bit pointer at image address `at`

/// ELF only: `Desc` a TLS descriptor pair at `got_at` (`R_AARCH64_TLSDESC`), `TpRel` a cell at
/// `got_at` holding the thread-pointer offset (`R_AARCH64_TLS_TPREL64`, `DF_STATIC_TLS`); both
/// of the variable at `offset` in the image's TLS template.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsReloc { Desc { got_at: u64, offset: u64 }, TpRel { got_at: u64, offset: u64 } } // ELF only

pub struct Image<'a> {
    pub platform: Platform,
    pub regions: AVec<'a, Placed>,        // in address order, page-aligned where a segment changes
    pub bytes: AVec<'a, u8>,              // file bytes from address `file_start` to the end of initialized data
    pub file_start: u64,                  // = Layout::header_reserve
    pub end: u64,                         // end of zero-fill
    pub fixups: AVec<'a, Fixup>,          // sorted by `at`
    pub tls: AVec<'a, TlsReloc>,
    pub imports: AVec<'a, Import<'a>>,
    pub exports: AVec<'a, (&'a [u8], u64)>, // roots it defines, plus `__blaze_native_v1`; sorted by name
    pub words: AVec<'a, &'a [u8]>,
    pub inits: u32,
    pub descriptor_at: u64,
    pub libraries: AVec<'a, Linked<'a>>, // Plan::libraries, load order
    /// Mach-O: the unwind rows the `UnwindInfo` region encodes, as data (sorted by start; a
    /// DWARF-mode row's low 24 bits are its FDE's offset in the `EhFrame` region). Empty on ELF.
    /// An executable writer (blaze-export eh_out.rs, W9) builds its one `__unwind_info` from them.
    pub unwind: AVec<'a, Row>,
    /// Every FDE of the `EhFrame` region, sorted by `at`.
    pub frames: AVec<'a, Frame>,
    /// Every pc-relative field of the `EhFrame` region, sorted by `at`.
    pub eh_pointers: AVec<'a, EhPointer>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Import<'a> { pub name: &'a [u8], pub weak: bool }

/// One FDE of the merged eh_frame, at image addresses: the record `[at, at + len)` in the
/// `EhFrame` region (its length field included), and the code `[start, start + range)` it
/// describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame { pub at: u64, pub len: u32, pub start: u64, pub range: u32 }

/// A pc-relative field of the merged eh_frame (an FDE's `pc_begin`, an LSDA or personality
/// pointer): `width` bytes (4 or 8, signed) at `at`, holding `to - minus`, both image
/// addresses (`minus` is `at` for a plain pc-relative field; another place of the eh_frame for a
/// Mach-O subtractor pair). A writer that moves the eh_frame's bytes away from what they point
/// at re-encodes each from these two addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EhPointer { pub at: u64, pub width: u8, pub to: u64, pub minus: u64 }

impl Image<'_> {
    /// The placed region `region` (every region is listed, so this always answers).
    pub fn region(&self, region: Region) -> Option<&Placed> {
        self.regions.iter().find(|p| p.region == region)
    }
}

/// The descriptor's tag and size: BNL2, 48 bytes; its form's one home is descriptor.rs.
pub use crate::descriptor::{LEN as DESCRIPTOR_LEN, MAGIC as DESCRIPTOR_MAGIC};
/// The descriptor's exported name as the symbol table holds it (C `__blaze_native_v1`; Mach-O
/// adds the leading `_`, so `dlsym(handle, "__blaze_native_v1")` finds it on both).
pub fn descriptor_name(platform: Platform) -> &'static [u8] {
    match platform {
        Platform::MacosArm64 => b"___blaze_native_v1",
        Platform::LinuxArm64 => b"__blaze_native_v1",
    }
}
