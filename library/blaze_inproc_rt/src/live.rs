//! What every dev executable that blaze-link linked live carries, whatever it depends on: the
//! image's descriptor (the symbol [`DESCRIPTOR`]) and its jump table's header ([`exec`]), as
//! the linker writes them and the program reads them, and the run side of the jump table that
//! the generated code and harness call by name:
//!
//! - [`log`]: the execution log, `__blaze_exec_slots` and `__blaze_exec_arm`, which the test
//!   harness's recording fork server (`blaze_usercode::test::fork_server(true)`) imports in every
//!   binary built with jump-table slots, and [`log::publish`], lazy compilation's placer.
//! - [`blaze_live_counters`]: the counters that code the daemon compiled to count its branches
//!   adds to (`hir_ty_codegen::tier::COUNTERS`).
//!
//! These were blaze-live's, so a dev test binary that did not depend on blaze-live had none
//! (starthook2): a binary with slots failed to link, naming `__blaze_exec_arm`. They are the dev
//! runtime's now, linked into every dev executable as the start hook is; blaze-live re-exports
//! them and builds the switch (taking patches) on them.
//!
//! [`find`] finds this image's descriptor and checks that an address range is in the image.

use std::ops::Range;

pub mod find;
pub mod log;
#[doc(hidden)]
pub mod mem;
#[cfg(test)]
mod tests;

/// The name of the descriptor's symbol, which the linker exports.
pub const DESCRIPTOR: &str = "__blaze_live";
pub const DESCRIPTOR_MAGIC: [u8; 8] = *b"BLZLIVE2";
/// The version of [`Descriptor`] this crate reads and writes: 4 since it names no directory
/// (patches come over the dev runtime's channel).
pub const DESCRIPTOR_VERSION: u32 = 4;

/// [`Descriptor::arch`] of x86-64 and of AArch64.
pub const ARCH_X86_64: u32 = 0;
pub const ARCH_AARCH64: u32 = 1;

/// [`Descriptor::arch`] of what this program runs on; none matches elsewhere.
pub const HOST_ARCH: u32 = if cfg!(target_arch = "x86_64") {
    ARCH_X86_64
} else if cfg!(target_arch = "aarch64") {
    ARCH_AARCH64
} else {
    u32::MAX
};

/// The section a live image's slot stubs are in, as blaze-link names it: Mach-O's and ELF's.
/// Slot 0's stub is at its start and slot `s`'s [`slot_stub_bytes`] times `s` past it; what
/// follows the last (the log stubs, lazy compilation's stubs) is no slot stub.
pub const STUBS_SECTION_MACHO: &str = "__blaze_stubs";
pub const STUBS_SECTION_ELF: &str = ".blaze.stubs";

/// The size of one slot's stub on `arch` ([`Descriptor::arch`]), as blaze-link writes it:
/// none for an architecture that has no stubs.
pub const fn slot_stub_bytes(arch: u32) -> Option<u64> {
    match arch {
        ARCH_X86_64 => Some(16),
        ARCH_AARCH64 => Some(24),
        _ => None,
    }
}

/// The slot whose stub `stub` (its [`slot_stub_bytes`] bytes as linked) is on `arch`, read
/// back from the offset of the slot's word (`slot * 8`) the stub loads: none when the bytes
/// are no slot stub of any form blaze-link writes. A function pointer of a live image is its
/// slot's stub (one stub a slot, so two pointers to one function compare equal), so a
/// profiler names a stub address by this.
///
/// - x86-64: `mov r11, [rip + cell]; jmp [r11 + disp32]; int3; int3`, the slot's offset the
///   `disp32` at byte 10.
/// - AArch64: `adrp x16, cell; ldr x16, [x16, :lo12:cell]`, then the slot's word loaded at
///   an offset that fits the load (`ldr x16, [x16, #imm12 * 8]`), or one `add x16, x16,
///   #imm12, lsl #12` before it, or built by `movz`/`movk x17` and `ldr x16, [x16, x17]`;
///   then `br x16`.
pub fn stub_slot(arch: u32, stub: &[u8]) -> Option<u32> {
    let offset = match arch {
        ARCH_X86_64 => {
            let stub = stub.get(..16)?;
            let fixed = stub[..3] == [0x4c, 0x8b, 0x1d]
                && stub[7..10] == [0x41, 0xff, 0xa3]
                && stub[14..] == [0xcc, 0xcc];
            if !fixed {
                return None;
            }
            u64::from(u32::from_le_bytes(stub[10..14].try_into().ok()?))
        }
        ARCH_AARCH64 => {
            let stub = stub.get(..24)?;
            let mut w = [0u32; 6];
            for (word, bytes) in w.iter_mut().zip(stub.chunks_exact(4)) {
                *word = u32::from_le_bytes(bytes.try_into().ok()?);
            }
            const ADRP_X16: (u32, u32) = (0x9f00_001f, 0x9000_0010);
            const LDR_X16: (u32, u32) = (0xffc0_03ff, 0xf940_0210);
            const ADD_X16_LSL12: (u32, u32) = (0xffc0_03ff, 0x9140_0210);
            const MOVZ_X17: (u32, u32) = (0xffe0_001f, 0xd280_0011);
            const MOVK_X17_LSL16: (u32, u32) = (0xffe0_001f, 0xf2a0_0011);
            const LDR_X16_X17: u32 = 0xf871_6a10;
            const BR_X16: u32 = 0xd61f_0200;
            let is = |word: u32, (mask, bits): (u32, u32)| word & mask == bits;
            let imm12 = |word: u32| u64::from((word >> 10) & 0xfff);
            let imm16 = |word: u32| u64::from((word >> 5) & 0xffff);
            if !is(w[0], ADRP_X16) || !is(w[1], LDR_X16) {
                return None;
            }
            if is(w[2], LDR_X16) && w[3] == BR_X16 {
                imm12(w[2]) << 3
            } else if is(w[2], ADD_X16_LSL12) && is(w[3], LDR_X16) && w[4] == BR_X16 {
                imm12(w[2]) << 12 | imm12(w[3]) << 3
            } else if is(w[2], MOVZ_X17)
                && is(w[3], MOVK_X17_LSL16)
                && w[4] == LDR_X16_X17
                && w[5] == BR_X16
            {
                imm16(w[2]) | imm16(w[3]) << 16
            } else {
                return None;
            }
        }
        _ => return None,
    };
    if !offset.is_multiple_of(8) {
        return None;
    }
    u32::try_from(offset / 8).ok()
}

/// The most slots a descriptor may name.
const MOST_SLOTS: u64 = 1 << 26;

/// The jump table's header, which [`Descriptor::table`] names, and its execution log
/// (fastcompile-ident-select 9): the layout blaze-link writes and the runtime reads, every
/// word little-endian, the header 8-aligned in writable data.
///
/// Every call through a slot loads [`CELL`](exec::CELL), then the slot's word of the table it
/// points at.
/// Without a log the cell points at the current generation's table. With one (blaze-link's
/// `Output::exec_log`), it points at the log table from the start: word `s` of it holds slot
/// `s`'s log stub until that is first called, which marks `map + 16 + s`, copies word `s` of
/// the table at [`CURRENT`](exec::CURRENT) into the log table's word `s` and jumps there, so
/// each later call goes straight to the code.
pub mod exec {
    /// The address of the table calls go through: the current generation's, or the log table.
    pub const CELL: u64 = 0;
    /// The address of the current generation's table, which the log stubs copy from.
    pub const CURRENT: u64 = 8;
    /// The address of the map the log stubs mark (slot `s` at `map + 16 + s`): the image's
    /// own private map until the process is armed, then the armed region.
    pub const MAP: u64 = 16;
    /// The address of slot 0's log stub; 0 in an image without a log.
    pub const STUBS: u64 = 24;
    /// The address of the loader's table, which holds the generation the image was linked at.
    pub const LOADER: u64 = 32;
    /// The table's slot count, a `u32`.
    pub const SLOTS: u64 = 40;
    /// The bytes from one log stub to the next, a `u32`; 0 without a log.
    pub const STRIDE: u64 = 44;
    /// The header's length without a log, and where the log table starts with one.
    pub const HEADER: u64 = 48;

    /// A region's first bytes: what `__blaze_exec_arm` checks it starts with.
    pub const MAGIC: [u8; 8] = *b"blzexec1";
    /// A map's header ([`MAGIC`], the slot count and the slots armed, each a `u32`); slot
    /// `s`'s byte follows at `REGION_HEADER + s`.
    pub const REGION_HEADER: u64 = 16;

    /// The offset of the log table's word for `slot`.
    pub const fn log_word(slot: u64) -> u64 {
        HEADER + slot * 8
    }

    /// The offset of the image's private map, after the log table of `slots` words.
    pub const fn private_map(slots: u64) -> u64 {
        HEADER + slots * 8
    }

    /// The header's length for a table of `slots` slots, with the log or without: the log
    /// table and the private map, padded to a word, follow the fixed words.
    pub const fn len(slots: u64, logged: bool) -> u64 {
        if logged {
            private_map(slots) + (REGION_HEADER + slots).next_multiple_of(8)
        } else {
            HEADER
        }
    }

    const _: () = assert!(len(3, true) == 48 + 24 + 24 && len(3, false) == HEADER);
}

/// What the symbol `__blaze_live` of an image blaze-link made is: written by the linker in
/// read-only data, every address as linked (unslid); the slide is where the descriptor is
/// in memory minus `this`.
#[repr(C)]
// clone: 96 bytes of plain data mirroring the symbol's, read once at start
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Descriptor {
    pub magic: [u8; 8],
    pub version: u32,
    /// 0 x86-64, 1 AArch64.
    pub arch: u32,
    pub id: [u8; 16],
    /// The generation the image was linked at: the program starts there, and accepts a patch
    /// whose `base` is the generation it is at.
    pub generation: u64,
    pub this: u64,
    /// The jump table's header ([`exec`]), whose first word is the cell holding the address
    /// of the table calls go through (an array of `slots` code addresses, slid by the
    /// loader); every call through a slot loads this cell first.
    pub table: u64,
    /// The table's capacity: every slot has its stub from the full link on, so a patch only
    /// sets cells.
    pub slots: u64,
    /// `[start, end)` of the region new code is placed in, and of new data's.
    pub patch_code: [u64; 2],
    pub patch_data: [u64; 2],
}

impl Descriptor {
    /// Its encoded size, which is also its size in memory: every field is aligned as it is.
    pub const SIZE: usize = 8 + 4 + 4 + 16 + 8 + 8 + 8 + 8 + 16 + 16;

    pub fn encode(&self) -> Vec<u8> {
        // alloc: the linker's, once per live image
        let mut bytes = Vec::with_capacity(Self::SIZE);
        bytes.extend_from_slice(&self.magic);
        bytes.extend_from_slice(&self.version.to_le_bytes());
        bytes.extend_from_slice(&self.arch.to_le_bytes());
        bytes.extend_from_slice(&self.id);
        for value in [self.generation, self.this, self.table, self.slots]
            .into_iter()
            .chain(self.patch_code)
            .chain(self.patch_data)
        {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    /// The descriptor at the start of `bytes`: none when they are too short or don't start
    /// with [`DESCRIPTOR_MAGIC`]. The version is the reader's to check.
    pub fn decode(bytes: &[u8]) -> Option<Descriptor> {
        let bytes = bytes.get(..Self::SIZE)?;
        let array = |at: usize| -> Option<[u8; 8]> { bytes.get(at..at + 8)?.try_into().ok() };
        let u32_at = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
        };
        let u64_at = |at: usize| array(at).map(u64::from_le_bytes);
        let magic = array(0)?;
        if magic != DESCRIPTOR_MAGIC {
            return None;
        }
        Some(Descriptor {
            magic,
            version: u32_at(8)?,
            arch: u32_at(12)?,
            id: bytes.get(16..32)?.try_into().ok()?,
            generation: u64_at(32)?,
            this: u64_at(40)?,
            table: u64_at(48)?,
            slots: u64_at(56)?,
            patch_code: [u64_at(64)?, u64_at(72)?],
            patch_data: [u64_at(80)?, u64_at(88)?],
        })
    }
}

const _: () = assert!(Descriptor::SIZE == size_of::<Descriptor>());

/// What an image's descriptor says, with its slide. Addresses are as linked unless a method
/// slides them. 80 bytes, made once at start.
#[derive(Debug)]
pub struct Layout {
    pub id: [u8; 16],
    /// The generation the image was linked at.
    pub generation: u64,
    pub slide: u64,
    /// The cell holding the current table's address.
    pub table: u64,
    pub slots: u64,
    pub code: Range<u64>,
    pub data: Range<u64>,
}

impl Layout {
    /// The layout of the descriptor found in memory at `at`.
    pub fn new(descriptor: &Descriptor, at: u64) -> Result<Layout, String> {
        if descriptor.version != DESCRIPTOR_VERSION {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR} is of version {}, where this runtime reads {DESCRIPTOR_VERSION}",
                descriptor.version
            ));
        }
        if descriptor.arch != HOST_ARCH {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR} is for architecture {}, where this program runs on {}",
                descriptor.arch,
                std::env::consts::ARCH
            ));
        }
        let [code_start, code_end] = descriptor.patch_code;
        let [data_start, data_end] = descriptor.patch_data;
        if code_start > code_end || data_start > data_end {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR} has a patch region that ends before it starts: code {code_start:#x}..\
                 {code_end:#x}, data {data_start:#x}..{data_end:#x}"
            ));
        }
        if descriptor.slots > MOST_SLOTS {
            // alloc: error path
            return Err(format!("{DESCRIPTOR} has {} slots", descriptor.slots));
        }
        let slide = at.wrapping_sub(descriptor.this);
        if !descriptor.table.wrapping_add(slide).is_multiple_of(8) {
            // alloc: error path
            return Err(format!(
                "{DESCRIPTOR}'s table cell at {:#x} is not aligned",
                descriptor.table
            ));
        }
        Ok(Layout {
            id: descriptor.id,
            generation: descriptor.generation,
            slide,
            table: descriptor.table,
            slots: descriptor.slots,
            code: code_start..code_end,
            data: data_start..data_end,
        })
    }

    pub fn slid(&self, address: u64) -> u64 {
        address.wrapping_add(self.slide)
    }

    /// Where the patch code region is in memory.
    pub fn code_in_memory(&self) -> Range<u64> {
        self.slid(self.code.start)..self.slid(self.code.end)
    }

    /// Where the table cell is in memory.
    pub fn cell(&self) -> u64 {
        self.slid(self.table)
    }
}

/// The descriptor of the image this code is in, found ([`find::descriptor`]), decoded and
/// checked: where it is in memory, and its layout.
pub fn this_layout() -> Result<(u64, Layout), String> {
    let (at, bytes) = find::descriptor()?;
    let descriptor = Descriptor::decode(bytes).ok_or_else(|| {
        // alloc: error path
        format!(
            "{DESCRIPTOR} at {at:#x} does not start with {}",
            // alloc: error path
            String::from_utf8_lossy(&DESCRIPTOR_MAGIC)
        )
    })?;
    Ok((at, Layout::new(&descriptor, at)?))
}

/// How many counters [`blaze_live_counters`] has room for.
pub const COUNTERS: usize = 1 << 16;

/// The counters that code the daemon compiled to count its branches adds to. That code finds
/// them by calling this, by name (`hir_ty_codegen::tier::COUNTERS`).
// SAFETY: the `blaze_live_` prefix is blaze's dev runtime's, linked once into a program (as a
// crate of the standard library, or as the workspace crate `blaze-inproc-rt`, never both), so
// no other item of a program exports `blaze_live_counters`; a second one would fail the link or
// bind the daemon's code to the wrong counters.
#[unsafe(no_mangle)]
pub extern "C" fn blaze_live_counters() -> *mut u64 {
    COUNTERS_CELLS.as_ptr() as *mut u64
}

/// [`blaze_live_counters`]'s: an `AtomicU64` is a `u64` in memory. The daemon's code must add
/// to them atomically (relaxed is enough).
static COUNTERS_CELLS: [std::sync::atomic::AtomicU64; COUNTERS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; COUNTERS];
