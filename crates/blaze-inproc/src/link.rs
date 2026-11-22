//! Deterministic in-process relocation patching and multi-function batch linking for
//! [`crate::exec::JitPool`].

use std::ops::Index;

use crate::Error;
use crate::exec::CodeHandle;

/// A relocation kind applied to code or data bytes inside a [`crate::exec::JitPool`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RelocKind {
    /// AArch64 26-bit signed PC-relative word branch (`B` or `BL`), range `[-128 MiB, +128 MiB - 4]`.
    ///
    /// Preserves bit 31 (`0` for `B`, `1` for `BL`) and requires a 4-byte aligned place and
    /// target.
    AArch64Branch26,
    /// AArch64 adjacent `ADRP Xd, #page` + `ADD Xd, Xn, #lo12` pair (8 bytes), range
    /// `[-4 GiB, +4 GiB - 4 KiB]` in 4 KiB pages.
    ///
    /// Preserves registers on both instructions and requires a 4-byte aligned place.
    AArch64AdrpAdd,
    /// x86-64 32-bit signed PC-relative displacement (`CALL`/`JMP`/`LEA`), computing
    /// `disp = (target + addend) - (place + 4)`, range `[i32::MIN, i32::MAX]`.
    X86Rel32,
    /// 64-bit little-endian absolute RX address (`target + addend`), for data or jump tables.
    Abs64,
}

impl RelocKind {
    /// Number of bytes read/written by this relocation.
    pub const fn width(self) -> usize {
        match self {
            RelocKind::AArch64Branch26 | RelocKind::X86Rel32 => 4,
            RelocKind::AArch64AdrpAdd | RelocKind::Abs64 => 8,
        }
    }

    /// Required alignment of `offset` within the unit's byte slice.
    pub const fn align(self) -> usize {
        match self {
            RelocKind::AArch64Branch26 | RelocKind::AArch64AdrpAdd => 4,
            RelocKind::X86Rel32 | RelocKind::Abs64 => 1,
        }
    }
}

/// The symbol or address a [`Reloc`] resolves to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RelocTarget {
    /// Another unit in the same [`crate::exec::JitPool::link_batch`] call, by index.
    Unit(usize),
    /// A previously allocated [`CodeHandle`] in the same [`crate::exec::JitPool`].
    Handle(CodeHandle),
    /// An absolute address (for example, an external host function or static table).
    Address(u64),
}

/// A single relocation entry within a [`LinkUnit`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Reloc {
    /// Byte offset inside the unit where the relocation applies.
    pub offset: usize,
    /// Relocation encoding and range rules.
    pub kind: RelocKind,
    /// Target symbol or address.
    pub target: RelocTarget,
    /// Signed byte addend added to the resolved target address.
    pub addend: i64,
}

impl Reloc {
    /// Creates a relocation with a zero addend.
    pub const fn new(offset: usize, kind: RelocKind, target: RelocTarget) -> Reloc {
        Reloc { offset, kind, target, addend: 0 }
    }

    /// Returns a copy of this relocation with `addend`.
    pub const fn with_addend(mut self, addend: i64) -> Reloc {
        self.addend = addend;
        self
    }
}

/// One function or data block submitted to [`crate::exec::JitPool::link_batch`].
#[derive(Clone, Copy, Debug)]
pub struct LinkUnit<'a> {
    /// Raw machine code or data bytes.
    pub bytes: &'a [u8],
    /// Minimum alignment in bytes (must be a power of two; clamped to at least 16B grain).
    pub align: u64,
    /// Relocations inside `bytes` to patch once every unit's RX address is known.
    pub relocs: &'a [Reloc],
}

impl<'a> LinkUnit<'a> {
    /// Creates a unit with the default 16-byte code alignment.
    pub const fn new(bytes: &'a [u8], relocs: &'a [Reloc]) -> LinkUnit<'a> {
        LinkUnit { bytes, align: 16, relocs }
    }

    /// Sets the requested alignment (must be a power of two).
    pub const fn with_align(mut self, align: u64) -> LinkUnit<'a> {
        self.align = align;
        self
    }
}

/// Handles returned by [`crate::exec::JitPool::link_batch`], one per input [`LinkUnit`] in the
/// same order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedBatch {
    handles: Vec<CodeHandle>,
}

impl LinkedBatch {
    pub(crate) fn new(handles: Vec<CodeHandle>) -> LinkedBatch {
        LinkedBatch { handles }
    }

    /// All handles allocated for the batch, in input unit order.
    pub fn handles(&self) -> &[CodeHandle] {
        &self.handles
    }

    /// The handle for unit `index`, if in bounds.
    pub fn handle(&self, index: usize) -> Option<CodeHandle> {
        self.handles.get(index).copied()
    }

    /// Number of units in the batch.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Whether the batch has no units.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Consumes the batch and returns its handle vector.
    pub fn into_handles(self) -> Vec<CodeHandle> {
        self.handles
    }
}

impl Index<usize> for LinkedBatch {
    type Output = CodeHandle;

    fn index(&self, index: usize) -> &CodeHandle {
        &self.handles[index]
    }
}

/// Applies one deterministic relocation in-place to `bytes`, assuming `bytes[0]` will execute at
/// RX address `block_rx`.
pub fn apply_reloc(
    bytes: &mut [u8],
    block_rx: u64,
    offset: usize,
    kind: RelocKind,
    target_rx: u64,
    addend: i64,
) -> Result<(), Error> {
    let width = kind.width();
    let end = offset.checked_add(width).ok_or_else(|| {
        Error::new(format!("relocation offset {offset:#x} + {width} overflows"))
    })?;
    if end > bytes.len() {
        return Err(Error::new(format!(
            "relocation {kind:?} at {offset:#x}..{end:#x} exceeds unit length {:#x}",
            bytes.len()
        )));
    }
    let req_align = kind.align();
    if !offset.is_multiple_of(req_align) {
        return Err(Error::new(format!(
            "relocation {kind:?} at offset {offset:#x} is not {req_align}-byte aligned"
        )));
    }
    let place = block_rx.checked_add(offset as u64).ok_or_else(|| {
        Error::new(format!("relocation place {block_rx:#x} + {offset:#x} overflows"))
    })?;
    let target_i128 = i128::from(target_rx) + i128::from(addend);
    let target = u64::try_from(target_i128).map_err(|_| {
        Error::new(format!(
            "relocation {kind:?} target {target_rx:#x} + ({addend}) overflows 64-bit address"
        ))
    })?;

    match kind {
        RelocKind::AArch64Branch26 => {
            if !place.is_multiple_of(4) || !target.is_multiple_of(4) {
                return Err(Error::new(format!(
                    "AArch64Branch26 requires 4-byte aligned place ({place:#x}) and target ({target:#x})"
                )));
            }
            let delta = i128::from(target) - i128::from(place);
            let imm26 = delta >> 2;
            const MIN_IMM26: i128 = -(1i128 << 25);
            const MAX_IMM26: i128 = (1i128 << 25) - 1;
            if !(MIN_IMM26..=MAX_IMM26).contains(&imm26) {
                return Err(Error::new(format!(
                    "AArch64Branch26 delta {delta:#x} ({place:#x} -> {target:#x}) out of ±128 MiB range"
                )));
            }
            let insn = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            if insn & 0x7c00_0000 != 0x1400_0000 {
                return Err(Error::new(format!(
                    "AArch64Branch26 at {offset:#x} expects B or BL instruction, found {insn:#010x}"
                )));
            }
            let patched = (insn & 0xfc00_0000) | ((imm26 as u32) & 0x03ff_ffff);
            bytes[offset..offset + 4].copy_from_slice(&patched.to_le_bytes());
            Ok(())
        }
        RelocKind::AArch64AdrpAdd => {
            if !place.is_multiple_of(4) {
                return Err(Error::new(format!(
                    "AArch64AdrpAdd requires 4-byte aligned place ({place:#x})"
                )));
            }
            let target_page = i128::from(target >> 12);
            let place_page = i128::from(place >> 12);
            let page_delta = target_page - place_page;
            const MIN_PAGES: i128 = -(1i128 << 20);
            const MAX_PAGES: i128 = (1i128 << 20) - 1;
            if !(MIN_PAGES..=MAX_PAGES).contains(&page_delta) {
                return Err(Error::new(format!(
                    "AArch64AdrpAdd page delta {page_delta} ({place:#x} -> {target:#x}) out of ±4 GiB range"
                )));
            }
            let adrp = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            if adrp & 0x9f00_0000 != 0x9000_0000 {
                return Err(Error::new(format!(
                    "AArch64AdrpAdd at {offset:#x} expects ADRP instruction, found {adrp:#010x}"
                )));
            }
            let add = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]);
            if add & 0xffc0_0000 != 0x9100_0000 {
                return Err(Error::new(format!(
                    "AArch64AdrpAdd at {:#x} expects 64-bit unshifted ADD immediate instruction, found {add:#010x}",
                    offset + 4
                )));
            }
            let delta_u32 = page_delta as u32;
            let immlo = delta_u32 & 0x3;
            let immhi = (delta_u32 >> 2) & 0x7_ffff;
            let patched_adrp = (adrp & 0x9f00_001f) | (immlo << 29) | (immhi << 5);
            let lo12 = (target & 0xfff) as u32;
            let patched_add = (add & !0x003f_fc00) | (lo12 << 10);
            bytes[offset..offset + 4].copy_from_slice(&patched_adrp.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&patched_add.to_le_bytes());
            Ok(())
        }
        RelocKind::X86Rel32 => {
            let next_rip = place.checked_add(4).ok_or_else(|| {
                Error::new(format!("X86Rel32 next_rip {place:#x} + 4 overflows"))
            })?;
            let disp = i128::from(target) - i128::from(next_rip);
            let disp32 = i32::try_from(disp).map_err(|_| {
                Error::new(format!(
                    "X86Rel32 displacement {disp:#x} ({place:#x} -> {target:#x}) out of i32 range"
                ))
            })?;
            bytes[offset..offset + 4].copy_from_slice(&disp32.to_le_bytes());
            Ok(())
        }
        RelocKind::Abs64 => {
            bytes[offset..offset + 8].copy_from_slice(&target.to_le_bytes());
            Ok(())
        }
    }
}
