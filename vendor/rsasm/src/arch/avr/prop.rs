//! `.avr.prop`: where the code sections were padded, for the linker.
//!
//! Linker relaxation deletes bytes from code, which would move an `.align`
//! off its boundary and an `.org` off its address. So GNU as for AVR records
//! each of them (`avr_create_and_fill_property_section` in
//! `gas/config/tc-avr.c`) and the linker reads them back
//! (`avr_elf32_load_property_records` in `bfd/elf32-avr.c`). The section is
//! written whenever there is something to record, which is whenever a code
//! section has an `.align` or `.org` in it.
//!
//! The format, all little-endian: a version byte (1), a flags byte (0) and a
//! 16-bit record count, then each record as a 32-bit address, relocated
//! against its section, a type byte, and the type's data:
//!
//! | type | meaning | data |
//! |---|---|---|
//! | 0 | `.org` | none |
//! | 1 | `.org` with a fill | the fill, 4 bytes |
//! | 2 | `.align` | the alignment as a power of two, 4 bytes |
//! | 3 | `.align` with a fill | the power, then the fill, 4 bytes each |
//!
//! The address is where the padding ends. Every alignment frag with a
//! nonzero power counts, including the one GNU as adds to round a code
//! section's end up to its alignment, and one that pads nothing because the
//! code is already aligned; an `.org` counts only where its target has a
//! nonzero constant part (`avr_handle_align` tests the frag's offset). A fill
//! counts only where it is not zero, and is stored sign-extended from a byte,
//! as GNU as's `char` holds it on the hosts it runs on.

use crate::arch::{LayoutPlace, LayoutRecords, PlaceKind};

const VERSION: u8 = 1;

const RECORD_ORG: u8 = 0;
const RECORD_ORG_AND_FILL: u8 = 1;
const RECORD_ALIGN: u8 = 2;
const RECORD_ALIGN_AND_FILL: u8 = 3;

pub fn records(places: &[LayoutPlace]) -> Option<LayoutRecords> {
    let kept: Vec<usize> = (0..places.len())
        .filter(|&i| match places[i].kind {
            PlaceKind::Align(power) => power > 0,
            PlaceKind::Org(offset) => offset > 0,
        })
        .collect();
    if kept.is_empty() {
        return None;
    }
    let mut bytes = vec![VERSION, 0];
    bytes.extend_from_slice(&(kept.len() as u16).to_le_bytes());
    let mut refs = Vec::with_capacity(kept.len());
    for i in kept {
        let p = places[i];
        refs.push((bytes.len() as u32, i));
        bytes.extend_from_slice(&[0; 4]);
        let fill = (p.fill != 0).then(|| i32::from(p.fill as i8).to_le_bytes());
        match p.kind {
            PlaceKind::Align(power) => {
                bytes.push(if fill.is_some() {
                    RECORD_ALIGN_AND_FILL
                } else {
                    RECORD_ALIGN
                });
                bytes.extend_from_slice(&power.to_le_bytes());
            }
            PlaceKind::Org(_) => {
                bytes.push(if fill.is_some() {
                    RECORD_ORG_AND_FILL
                } else {
                    RECORD_ORG
                });
            }
        }
        if let Some(fill) = fill {
            bytes.extend_from_slice(&fill);
        }
    }
    Some(LayoutRecords {
        name: ".avr.prop",
        bytes,
        refs,
    })
}
