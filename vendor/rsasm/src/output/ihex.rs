//! Intel HEX output: the flat image [`super::raw`] builds, as text records.
//!
//! The records are laid out as llvm-objcopy's Intel HEX writer lays out a
//! binary it is given, which is what `tools/xas-diff` checks them against, and
//! for an image without gaps they are also what the Macro Assembler AS's
//! `p2hex` writes: sixteen data bytes to a record from the image's first
//! address, a record never crossing a 64 KiB boundary, an extended segment
//! address record (type 02) for an address below 1 MiB that needs one and an
//! extended linear address record (type 04) above it, and an end-of-file
//! record. No start address is written.
//!
//! The gaps between the pieces of an image are written as the zeros `-f bin`
//! puts there, so the two formats load the same bytes. AS's `p2hex` and
//! sdld leave reserved space out instead.

use super::OutputError;
use crate::assembler::Assembler;

/// Data bytes per record.
const RECORD: usize = 16;

/// Builds the Intel HEX text of the flat image.
pub fn build(asm: &Assembler) -> Result<Vec<u8>, OutputError> {
    let image = super::raw::build(asm)?;
    let start = super::raw::image_start(asm);
    encode(start, &image)
}

/// The Intel HEX records for `image` loaded at `start`.
/// Not API.
#[doc(hidden)]
pub fn encode(start: u64, image: &[u8]) -> Result<Vec<u8>, OutputError> {
    let end = start + image.len() as u64;
    if end > 1 << 32 {
        return Err(OutputError::Unsupported(format!(
            "Intel HEX addresses 4 GiB; this image ends at {end:#x}"
        )));
    }
    let mut out = String::new();
    // The base the extended address records have set, and whether the last
    // one was a segment record, which a linear one has to cancel.
    let mut base = 0u64;
    let mut segment = false;
    let mut addr = start;
    let mut rest = image;
    while !rest.is_empty() {
        let window = addr & !0xffff;
        if window != base {
            if addr < 1 << 20 {
                record(&mut out, 0, 0x02, &[(window >> 12) as u8, 0]);
                segment = window != 0;
            } else {
                if segment {
                    record(&mut out, 0, 0x02, &[0, 0]);
                    segment = false;
                }
                record(
                    &mut out,
                    0,
                    0x04,
                    &[(window >> 24) as u8, (window >> 16) as u8],
                );
            }
            base = window;
        }
        let room = (window + 0x10000 - addr) as usize;
        let n = rest.len().min(RECORD).min(room);
        record(&mut out, (addr & 0xffff) as u16, 0x00, &rest[..n]);
        rest = &rest[n..];
        addr += n as u64;
    }
    record(&mut out, 0, 0x01, &[]);
    Ok(out.into_bytes())
}

/// Appends one record: `:`, the byte count, the 16-bit address, the type,
/// the data and the two's-complement checksum of all of those bytes.
fn record(out: &mut String, addr: u16, kind: u8, data: &[u8]) {
    use std::fmt::Write;
    let head = [data.len() as u8, (addr >> 8) as u8, addr as u8, kind];
    let sum = head.iter().chain(data).fold(0u8, |s, b| s.wrapping_add(*b));
    out.push(':');
    for b in head.iter().chain(data) {
        let _ = write!(out, "{b:02X}");
    }
    let _ = writeln!(out, "{:02X}", sum.wrapping_neg());
}

#[cfg(test)]
mod tests {
    use super::encode;

    fn text(start: u64, image: &[u8]) -> String {
        String::from_utf8(encode(start, image).unwrap()).unwrap()
    }

    // The expected text in these was written by llvm-objcopy 22, `-I binary
    // -O ihex --change-section-address .data=<start>`, from the same bytes.

    #[test]
    fn records_run_sixteen_bytes_from_the_first_address() {
        let image: Vec<u8> = (1..=12).flat_map(|i| [0x74, i]).collect();
        assert_eq!(
            text(0x105, &image),
            ":100105007401740274037404740574067407740826\n\
             :080115007409740A740B740CE8\n\
             :00000001FF\n"
        );
    }

    #[test]
    fn an_address_past_64k_takes_a_segment_record() {
        assert_eq!(
            text(0x1fff8, &[0; 24]),
            ":020000021000EC\n\
             :08FFF800000000000000000001\n\
             :020000022000DC\n\
             :1000000000000000000000000000000000000000F0\n\
             :00000001FF\n"
        );
    }

    #[test]
    fn an_address_past_1m_takes_a_linear_record() {
        assert_eq!(
            text(0xffff8, &[0; 24]),
            ":02000002F0000C\n\
             :08FFF800000000000000000001\n\
             :020000020000FC\n\
             :020000040010EA\n\
             :1000000000000000000000000000000000000000F0\n\
             :00000001FF\n"
        );
        assert_eq!(
            text(0x12345678, &[0; 8]),
            ":020000041234B4\n\
             :0856780000000000000000002A\n\
             :00000001FF\n"
        );
    }

    #[test]
    fn an_empty_image_is_just_the_end_record() {
        assert_eq!(text(0, &[]), ":00000001FF\n");
    }
}
