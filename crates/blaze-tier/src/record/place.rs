//! [`place`]: one `bzcode08` record made code at an address in a running program, completing
//! each relocation by its kind. The model is blaze-code place.rs's `place` for `bzcode01`, whose
//! [`Words`] and [`At`] it reuses, and whose answers it gives: a call loads the callee's SLOT
//! word, a load of a function's address its entry thunk's word, an address or relro word a
//! function's entry thunk (never a body: a later tier or a freed body never leaves a pointer to
//! stale code). A displacement out of range is refused by name, never relaxed.
//!
//! Which import word a GOT pair loads (`GotPage21` on an `adrp`, `GotPageOff12` on the `ldr` of
//! the same register, target and addend after it): the callee's slot word when the `ldr` loads
//! `x16` or `x17` and the next instruction is `blr` of that register (a call: the value dies at
//! the `blr`, since x16 and x17 are not kept across a call), else the word holding the target's
//! entry thunk (a load). An `adrp` takes the word its first such `ldr` takes; an `ldr` whose
//! `adrp` also serves an `ldr` of the other answer is refused by name. Treating a load as a call
//! could leave a stale pointer; the rule only calls what the next instruction calls at once.
//!
//! Pure: it writes the bytes the caller hands it and nothing else; the caller does the cache
//! maintenance and the word's release store, as for `bzcode01`.

use blaze_code::a64::{ADD64, ADRP, LDR64, adrp, expect, ldr_lo12, read32, write32};
use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{OWN_CODE, OWN_RELRO, PositionPart, Space, Symbol};

use super::form::RecordView;
use super::{Reloc, RelocKind};

/// `blr Xn` without its register.
const BLR_BASE: u32 = 0xD63F_0000;
/// `b`/`bl`: bits 30..26 fixed, bit 31 the link.
const B_OR_BL: (u32, u32) = (0x7C00_0000, 0x1400_0000);
/// Load/store register, unsigned immediate offset: bits 29..27 `111`, 25..24 `01`.
const LDST_UIMM: (u32, u32) = (0x3B00_0000, 0x3900_0000);

/// [`place_resolved`] of a record with no source-position slot: one with any is refused naming
/// its rows (`PlaceError::PositionBytes`), since only the run's answers fill them.
pub fn place(
    view: &RecordView<'_>,
    symbol: Symbol,
    at: At,
    words: &mut dyn Words,
    code: &mut [u8],
    relro: &mut [u8],
) -> Result<(), PlaceError> {
    place_resolved(view, symbol, at, words, code, relro, &[])
}

/// Places `view` (function `symbol`) at `at`: copies its code into `code` and its relro part into
/// `relro` (the write view's bytes at `at.code` and `at.relro`), fills each source-position
/// slot with its answer in `values` (one little-endian `u32` per slot, in the view's slot order:
/// the parent's, blaze-cli run_parent.rs `position_values`), completes every relocation by its
/// kind against the run's words, and fills every relro word. Each part's address must honour
/// the record's alignment (`code_align`, `relro_align`). Deterministic: the same record, `at`,
/// `values` and answers of `words` give the same bytes. x86-64's kinds (`Pc32`, `GotPc32`) are
/// refused by name: this is the arm64 placer.
pub fn place_resolved(
    view: &RecordView<'_>,
    symbol: Symbol,
    at: At,
    words: &mut dyn Words,
    code: &mut [u8],
    relro: &mut [u8],
    values: &[u8],
) -> Result<(), PlaceError> {
    let want = view.code().len();
    if code.len() != want {
        return Err(PlaceError::Size { symbol, what: "code", want, got: code.len() });
    }
    let want = view.relro().len();
    if relro.len() != want {
        return Err(PlaceError::Size { symbol, what: "relro", want, got: relro.len() });
    }
    if view.position_slots().checked_mul(4) != Some(values.len()) {
        return Err(PlaceError::PositionBytes { symbol, rows: view.position_slots(), bytes: values.len() });
    }
    // Each part where its record says: a data row aligned to 64 is at a multiple of 64 from its
    // part's first byte, so only that byte's alignment makes the row's address one.
    let align = 1u64 << view.code_align();
    if at.code % align != 0 {
        return Err(PlaceError::Misaligned { symbol, what: "the code", at: at.code, align });
    }
    let align = 1u64 << view.relro_align();
    if at.relro % align != 0 {
        return Err(PlaceError::Misaligned { symbol, what: "the relro part", at: at.relro, align });
    }
    code.copy_from_slice(view.code());
    relro.copy_from_slice(view.relro());
    // The slots first: the seal proved none shares a byte with a relocation or relro word.
    for i in 0..view.position_slots() {
        let row = view.position_slot(i);
        let resolved = read32(values, (i * 4) as u64);
        if resolved == 0 {
            return Err(PlaceError::Site(ImageError::ZeroPosition { symbol, row }));
        }
        let part = match row.part {
            PositionPart::Code => &mut *code,
            PositionPart::Relro => &mut *relro,
        };
        write32(part, u64::from(row.at), resolved);
    }
    for i in 0..view.relocs() {
        let r = view.reloc(i);
        let off = u64::from(r.at);
        let pc = at.code + off;
        match r.kind {
            RelocKind::Page21 => {
                let inst = expect(code, off, ADRP, symbol, "adrp")?;
                let to = address(view, words, at, r)?;
                write32(code, off, adrp(inst, pc, to, symbol)?);
            }
            RelocKind::PageOff12 { scale } => {
                let inst = read32(code, off);
                let to = address(view, words, at, r)?;
                write32(code, off, page_offset(inst, scale, to, off, symbol)?);
            }
            RelocKind::Branch26 => {
                let inst = expect(code, off, B_OR_BL, symbol, "b or bl")?;
                let to = address(view, words, at, r)?;
                let delta = to as i64 - pc as i64;
                if delta % 4 != 0 || !(-(1i64 << 27)..(1i64 << 27)).contains(&delta) {
                    return Err(PlaceError::Site(ImageError::OutOfReach { symbol, at: off, what: "b/bl (128 MiB)" }));
                }
                write32(code, off, (inst & 0xFC00_0000) | (((delta >> 2) as u32) & 0x03FF_FFFF));
            }
            RelocKind::GotPage21 => {
                let inst = expect(code, off, ADRP, symbol, "adrp")?;
                let word = import_word(view, words, i, symbol)?;
                write32(code, off, adrp(inst, pc, word, symbol)?);
            }
            RelocKind::GotPageOff12 => {
                let inst = expect(code, off, LDR64, symbol, "ldr")?;
                let word = import_word(view, words, i, symbol)?;
                write32(code, off, ldr_lo12(inst, word));
            }
            RelocKind::Abs64 => {
                let value = address(view, words, at, r)?;
                let start = r.at as usize;
                relro[start..start + 8].copy_from_slice(&value.to_le_bytes());
            }
            RelocKind::Pc32 | RelocKind::GotPc32 => {
                return Err(PlaceError::Site(ImageError::NotInstruction {
                    symbol,
                    at: off,
                    want: "arm64 relocation (Pc32 and GotPc32 are x86-64's; the arm64 placer refuses them)",
                }));
            }
        }
    }
    for w in 0..view.words() {
        let word = view.word(w);
        let value = match word.target {
            OWN_CODE => at.code,
            OWN_RELRO => at.relro,
            index => {
                let target = view.target(index as usize);
                if target.space == Space::Outside && word.addend != 0 {
                    return Err(PlaceError::Site(ImageError::OutsideAddend { symbol: target.symbol }));
                }
                words.value(target)?
            }
        };
        let filled = value.wrapping_add_signed(word.addend);
        let start = word.at as usize;
        relro[start..start + 8].copy_from_slice(&filled.to_le_bytes());
    }
    Ok(())
}

/// The address an address relocation names, its addend added: the record's own code or relro
/// byte, or the target's value (a function's entry thunk, a static's or vtable's address, a C
/// name's address; for `Abs64` on a thread-local, its offset). The seal refused a thread-local
/// under any other kind and an outside name with an addend.
fn address(view: &RecordView<'_>, words: &mut dyn Words, at: At, r: Reloc) -> Result<u64, PlaceError> {
    let base = match r.target {
        OWN_CODE => at.code,
        OWN_RELRO => at.relro,
        index => words.value(view.target(index as usize))?,
    };
    Ok(base.wrapping_add_signed(r.addend))
}

/// The page offset of `to` in an `add` (scale 0) or a load or store of `1 << scale` bytes.
/// Public for blaze-export optimized.rs, which completes `PageOff12` the same way.
pub fn page_offset(inst: u32, scale: u8, to: u64, off: u64, symbol: Symbol) -> Result<u32, PlaceError> {
    let is_add = inst & ADD64.0 == ADD64.1;
    let is_ldst = inst & LDST_UIMM.0 == LDST_UIMM.1;
    let fits = match (is_add, is_ldst) {
        (true, _) => scale == 0,
        (false, true) => access_log2(inst) == u32::from(scale),
        (false, false) => false,
    };
    if !fits {
        return Err(PlaceError::Site(ImageError::NotInstruction {
            symbol,
            at: off,
            want: "add, or load or store (unsigned offset) of the page offset's size",
        }));
    }
    let lo = to & 0xFFF;
    let align = 1u64 << scale;
    if lo % align != 0 {
        return Err(PlaceError::Misaligned { symbol, what: "a page offset's target", at: to, align });
    }
    Ok((inst & !(0xFFF << 10)) | (((lo >> scale) as u32) << 10))
}

/// The log2 of the bytes a load or store (unsigned offset) moves: `size`, or 4 for a 128-bit
/// vector register (`V` set, `size` 0, `opc` bit 1 set).
fn access_log2(inst: u32) -> u32 {
    let size = inst >> 30;
    let vector = (inst >> 26) & 1 == 1;
    let opc_high = (inst >> 23) & 1 == 1;
    match vector && size == 0 && opc_high {
        true => 4,
        false => size,
    }
}

/// Whether the `ldr` at `ldr_at` loads x16 or x17 and the next instruction is `blr` of it.
/// Public for blaze-export optimized.rs, which relaxes exactly these GOT pairs into `bl`.
pub fn calls(code: &[u8], ldr_at: u32) -> bool {
    let ldr = read32(code, u64::from(ldr_at));
    let t = ldr & 0x1F;
    if t != 16 && t != 17 {
        return false;
    }
    let next = u64::from(ldr_at) + 4;
    next + 4 <= code.len() as u64 && read32(code, next) == BLR_BASE | (t << 5)
}

/// Whether the `adrp` at `page` and the `ldr` at `off` are one GOT pair: same target and addend,
/// the `ldr`'s base the `adrp`'s register.
fn pair(code: &[u8], page: Reloc, off: Reloc) -> bool {
    let rd = read32(code, u64::from(page.at)) & 0x1F;
    let rn = (read32(code, u64::from(off.at)) >> 5) & 0x1F;
    page.target == off.target && page.addend == off.addend && rd == rn
}

/// The first `GotPageOff12` after the `GotPage21` relocation `i` that pairs with it.
fn first_off(view: &RecordView<'_>, i: usize) -> Option<usize> {
    let code = view.code();
    let page = view.reloc(i);
    (i + 1..view.relocs()).find(|&j| {
        let off = view.reloc(j);
        off.kind == RelocKind::GotPageOff12 && pair(code, page, off)
    })
}

/// The last `GotPage21` before the `GotPageOff12` relocation `j` that pairs with it.
fn last_page(view: &RecordView<'_>, j: usize) -> Option<usize> {
    let code = view.code();
    let off = view.reloc(j);
    (0..j).rev().find(|&i| {
        let page = view.reloc(i);
        page.kind == RelocKind::GotPage21 && pair(code, page, off)
    })
}

/// The import word GOT relocation `i` reaches (the module's documentation says which).
fn import_word(view: &RecordView<'_>, words: &mut dyn Words, i: usize, symbol: Symbol) -> Result<u64, PlaceError> {
    let code = view.code();
    let r = view.reloc(i);
    let unpaired = PlaceError::Site(ImageError::NotInstruction {
        symbol,
        at: u64::from(r.at),
        want: "GOT pair (an adrp and the ldr of the same register, target and addend after it)",
    });
    let call = match r.kind {
        RelocKind::GotPage21 => {
            let j = first_off(view, i).ok_or(unpaired)?;
            calls(code, view.reloc(j).at)
        }
        _ => {
            let page = last_page(view, i).ok_or(unpaired)?;
            let first = first_off(view, page).ok_or(unpaired)?;
            let mine = calls(code, r.at);
            if calls(code, view.reloc(first).at) != mine {
                return Err(PlaceError::Site(ImageError::NotInstruction {
                    symbol,
                    at: u64::from(r.at),
                    want: "GOT pair whose adrp serves only calls or only loads",
                }));
            }
            mine
        }
    };
    let target = view.target(r.target as usize);
    let word = words.word(target, call)?;
    if word % 8 != 0 {
        return Err(PlaceError::Misaligned { symbol, what: "a target's word", at: word, align: 8 });
    }
    Ok(word)
}
