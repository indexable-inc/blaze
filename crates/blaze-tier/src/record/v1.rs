//! [`from_v1`]: a `bzcode01` record (blaze-code's `CodeRecord`, what the Reference tier, today's
//! cell emitter, makes) in the new form, so the Reference tier enters the one form unchanged and
//! is placed by the one new `place`. Retired with `bzcode01` when its last user has moved.
//!
//! Each paired site becomes relocations (D100 point 11):
//!
//! | `bzcode01` site | `bzcode02` relocations |
//! |---|---|
//! | `Load` at `a` | `GotPage21` at `a`, `GotPageOff12` at `a + 4`, the target |
//! | `Call` at `a` | the same, the `blr x16` at `a + 8` left in the code (it marks the pair a call) |
//! | `Relro` at `a`, byte `n` | `Page21` at `a`, `PageOff12 { scale: 0 }` at `a + 4`, `OWN_RELRO` plus `n` |
//!
//! A `Load` or `Call` site reaches the target's IMPORT WORD (blaze-code place.rs: the slot word
//! for a call, the entry thunk's word for a load), which is what piece 0's `GotPage21` and
//! `GotPageOff12` mean ("adrp of the target's import word", "ldr of the target's import word,
//! low 12 bits scaled by 8"); `Page21` and `PageOff12` reach the target's own address. The
//! relro words, the pads, the targets, the names, the parts' alignments, the trap rows and the
//! source-position slots are carried over as they are; the frame word becomes the unwind frame
//! and the pads also become its unwind rows. `bzcode01` holds no frame size, so `frame` is 0.
//! (`bzcode01` here is blaze-code's `CodeRecord`, whose form is now `bzcode07`.)

use blaze_code::record::{CodeRecord, CodeView, OWN_RELRO, SiteKind, Space};
use blaze_code::unwind;
use blaze_own::{AVec, Alloc, Vec};

use super::form::FormError;
use super::{Emitted, Reloc, RelocKind};

/// `record` in the new form, in `alloc` (the table in the module's documentation). The record is
/// sealed and read back through blaze-code's own checks first (its names are kept only there),
/// so what enters the new form has passed every `bzcode01` rule.
pub fn from_v1<'a>(record: CodeRecord<'a>, alloc: Alloc<'a>) -> Result<Emitted<'a>, FormError> {
    let sealed = record.seal().map_err(FormError::V1Record)?;
    let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
    sealed.encode(&mut bytes);
    drop(sealed);
    let view = CodeView::read(&bytes).map_err(FormError::V1Decode)?;
    let rows = unwind::read(view).map_err(FormError::Unwind)?;
    let mut e = Emitted::new_in(alloc);
    e.code.extend_from_slice(view.code());
    e.relro.extend_from_slice(view.relro());
    // The columns `bzcode08` shares with blaze-code's form, carried whole (nothing of the
    // source record is dropped): each part's alignment, the trap rows with their held values
    // and subjects, the source-position slots.
    e.code_align = view.code_align();
    e.relro_align = view.relro_align();
    for i in 0..view.traps() {
        e.traps.push(view.trap(i));
    }
    for i in 0..view.positions() {
        e.position_slots.push(view.position(i));
    }
    for i in 0..view.targets() {
        e.targets.push(view.target(i));
    }
    for i in 0..view.sites() {
        let site = view.site(i);
        let (page, off, target, addend) = match site.kind {
            SiteKind::Load | SiteKind::Call => (RelocKind::GotPage21, RelocKind::GotPageOff12, site.target, 0i64),
            SiteKind::Relro => (RelocKind::Page21, RelocKind::PageOff12 { scale: 0 }, OWN_RELRO, i64::from(site.addend)),
        };
        e.relocs.push(Reloc { at: site.at, in_relro: false, kind: page, target, addend });
        e.relocs.push(Reloc { at: site.at + 4, in_relro: false, kind: off, target, addend });
    }
    for i in 0..view.words() {
        e.words.push(view.word(i));
    }
    for i in 0..view.pads() {
        e.pads.push(view.pad(i));
    }
    e.unwind_frame = rows.frame();
    for i in 0..rows.rows() {
        e.unwind.push(rows.row(i));
    }
    for i in 0..view.targets() {
        let target = view.target(i);
        if target.space != Space::Outside {
            continue;
        }
        // The seal refused an outside target with no name (`RecordError::NameMissing`).
        let Some(name) = view.name(target.symbol) else {
            return Err(FormError::NameMissing { symbol: target.symbol });
        };
        let at = e.text.len() as u32;
        e.text.extend_from_slice(name.as_bytes());
        e.names.push((target.symbol, at, name.len() as u32));
    }
    Ok(e)
}
