//! The three readers on headers built here: what a run maps of a well-formed executable, each
//! refusal, and mutated or cut headers an error, never a panic.

use blaze_ids::rng::SplitMix64;

use super::{Layout, Segment, elf, headers_len, layout};
use crate::Format;

const PAGE: u64 = 0x1000;

fn put16(out: &mut Vec<u8>, at: usize, value: u16) {
    out[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// `PT_LOAD`s of (flags, offset, vaddr, filesz, memsz).
pub(crate) fn elf(loads: &[(u32, u64, u64, u64, u64)]) -> Vec<u8> {
    let mut out = vec![0u8; 64 + 56 * (loads.len() + 1)];
    out[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
    put16(&mut out, 16, 3);
    put16(&mut out, 18, elf::MACHINE);
    put64(&mut out, 32, 64);
    put16(&mut out, 54, 56);
    put16(&mut out, 56, loads.len() as u16 + 1);
    // A PT_GNU_STACK first, which the reader skips.
    put32(&mut out, 64, 0x6474_e551);
    for (index, &(flags, offset, vaddr, filesz, memsz)) in loads.iter().enumerate() {
        let at = 64 + 56 * (index + 1);
        put32(&mut out, at, 1);
        put32(&mut out, at + 4, flags);
        put64(&mut out, at + 8, offset);
        put64(&mut out, at + 16, vaddr);
        put64(&mut out, at + 24, vaddr);
        put64(&mut out, at + 32, filesz);
        put64(&mut out, at + 40, memsz);
        put64(&mut out, at + 48, PAGE);
    }
    out
}

pub(crate) const R: u32 = 4;
const RW: u32 = 6;
pub(crate) const RX: u32 = 5;

fn a_good_elf() -> Vec<u8> {
    elf(&[
        (R, 0, 0, 0x800, 0x800),
        (RX, 0x1000, 0x1000, 0x2345, 0x2345),
        (RW, 0x4000, 0x4000, 0x100, 0x3000),
        (0, 0, 0x8000, 0, 0x10_0000),
        (0, 0, 0x10_8000, 0, 0x4000),
    ])
}

#[test]
fn an_elf_gives_its_code_reservations_and_header() {
    let got = layout(Format::Elf, &a_good_elf(), PAGE).unwrap();
    assert_eq!(got, Layout {
        format: Format::Elf,
        header: 0,
        span: 0..0x10_c000,
        code: vec![Segment { offset: 0x1000, linked: 0x1000, len: 0x3000 }],
        reserved: vec![0x8000..0x10_8000, 0x10_8000..0x10_c000],
    });
    assert_eq!(headers_len(Format::Elf, &a_good_elf()), Some(64 + 56 * 6));
}

#[test]
fn an_elf_is_refused_for_what_a_run_cannot_overlay() {
    let refused = |loads: &[(u32, u64, u64, u64, u64)], says: &str| {
        let error = layout(Format::Elf, &elf(loads), PAGE).unwrap_err();
        assert!(error.what.contains(says), "{error} does not say {says:?}");
    };
    refused(&[(R, 0, 0, 0x800, 0x800), (RX | RW, 0x1000, 0x1000, 0x10, 0x10)], "writable and executable");
    refused(&[(R, 0, 0, 0x800, 0x800), (RX, 0x1000, 0x1000, 0x10, 0x20)], "all file bytes");
    refused(&[(R, 0, 0, 0x800, 0x800), (RX, 0x1010, 0x1010, 0x10, 0x10)], "does not start on a page");
    refused(&[(R, 0, 0, 0x800, 0x800), (RX, 0x1000, 0x1000, 0x10, 0x10), (RW, 0x1800, 0x1800, 8, 8)], "shares a page");
    refused(&[(R, 0, 0, 0x800, 0x800), (0, 0x1000, 0x1000, 0x10, 0x1000)], "has file bytes");
    refused(&[(R, 0, 0, 0x800, 0x800), (0, 0, 0x1800, 0, 0x1000)], "not whole pages");
    refused(&[(RX, 0x1000, 0x1000, 0x10, 0x10)], "file offset 0");
    let mut wrong = a_good_elf();
    put16(&mut wrong, 18, if elf::MACHINE == 62 { 183 } else { 62 });
    assert!(layout(Format::Elf, &wrong, PAGE).unwrap_err().what.contains("e_machine"));
    let mut many = a_good_elf();
    put16(&mut many, 56, 0xffff);
    assert!(layout(Format::Elf, &many, PAGE).is_err());
}

/// Segments of (name, vmaddr, vmsize, fileoff, filesize, initprot).
fn macho(segments: &[(&str, u64, u64, u64, u64, u32)]) -> Vec<u8> {
    let commands = 72 * segments.len();
    let mut out = vec![0u8; 32 + commands];
    put32(&mut out, 0, 0xfeed_facf);
    put32(&mut out, 4, 0x0100_000c);
    put32(&mut out, 12, 2);
    put32(&mut out, 16, segments.len() as u32);
    put32(&mut out, 20, commands as u32);
    for (index, &(name, vmaddr, vmsize, fileoff, filesize, initprot)) in segments.iter().enumerate() {
        let at = 32 + 72 * index;
        put32(&mut out, at, 0x19);
        put32(&mut out, at + 4, 72);
        out[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
        put64(&mut out, at + 24, vmaddr);
        put64(&mut out, at + 32, vmsize);
        put64(&mut out, at + 40, fileoff);
        put64(&mut out, at + 48, filesize);
        put32(&mut out, at + 56, 7);
        put32(&mut out, at + 60, initprot);
    }
    out
}

const MPAGE: u64 = 0x4000;

fn a_good_macho() -> Vec<u8> {
    macho(&[
        ("__PAGEZERO", 0, 0x1_0000_0000, 0, 0, 0),
        ("__TEXT", 0x1_0000_0000, 0x8000, 0, 0x8000, 5),
        ("__DATA", 0x1_0000_8000, 0x8000, 0x8000, 0x4000, 3),
        ("__BLAZE", 0x1_0001_0000, 0x40_0000, 0, 0, 0),
        ("__LINKEDIT", 0x1_0041_0000, 0x4000, 0xc000, 0x1000, 1),
    ])
}

#[test]
fn a_macho_gives_its_code_reservations_and_header() {
    let got = layout(Format::MachO, &a_good_macho(), MPAGE).unwrap();
    assert_eq!(got, Layout {
        format: Format::MachO,
        header: 0x1_0000_0000,
        span: 0x1_0000_0000..0x1_0041_4000,
        code: vec![Segment { offset: 0, linked: 0x1_0000_0000, len: 0x8000 }],
        reserved: vec![0x1_0001_0000..0x1_0041_0000],
    });
    assert_eq!(headers_len(Format::MachO, &a_good_macho()), Some(32 + 72 * 5));
}

#[test]
fn a_macho_is_refused_for_what_a_run_cannot_overlay() {
    let text = ("__TEXT", 0x1_0000_0000, 0x8000, 0, 0x8000, 5);
    let refused = |segments: &[(&str, u64, u64, u64, u64, u32)], says: &str| {
        let error = layout(Format::MachO, &macho(segments), MPAGE).unwrap_err();
        assert!(error.what.contains(says), "{error} does not say {says:?}");
    };
    refused(&[("__TEXT", 0x1_0000_0000, 0x8000, 0, 0x8000, 7)], "writable and executable");
    refused(&[("__TEXT", 0x1_0000_0000, 0x8000, 0, 0x4000, 5)], "all file bytes");
    refused(&[text, ("__X", 0x1_0000_8000, 0x4000, 0x8000, 0x10, 0)], "has file bytes");
    refused(&[text, ("__X", 0x1_0000_6000, 0x4000, 0x8000, 0x10, 1)], "shares a page");
    refused(&[("__DATA", 0x1_0000_0000, 0x4000, 0x4000, 0x4000, 3)], "file offset 0");
    let mut arm64e = macho(&[text]);
    put32(&mut arm64e, 8, 2);
    assert!(layout(Format::MachO, &arm64e, MPAGE).unwrap_err().what.contains("not arm64"));
    let mut long = macho(&[text]);
    put32(&mut long, 36, 4096);
    assert!(layout(Format::MachO, &long, MPAGE).is_err());
}

const GRANULE: u64 = 0x1_0000;
const BASE: u64 = 0x1_4000_0000;

/// Sections of (VirtualSize, VirtualAddress, SizeOfRawData, PointerToRawData, Characteristics).
fn pe(sections: &[(u32, u32, u32, u32, u32)], size: u32) -> Vec<u8> {
    let lfanew = 0x80;
    let optional = lfanew + 24;
    let table = optional + 240;
    let mut out = vec![0u8; table + 40 * sections.len()];
    out[..2].copy_from_slice(b"MZ");
    put32(&mut out, 0x3c, lfanew as u32);
    out[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");
    put16(&mut out, lfanew + 4, 0x8664);
    put16(&mut out, lfanew + 6, sections.len() as u16);
    put16(&mut out, lfanew + 20, 240);
    put16(&mut out, lfanew + 22, 0x0022);
    put16(&mut out, optional, 0x20b);
    put64(&mut out, optional + 24, BASE);
    put32(&mut out, optional + 32, 0x1000);
    put32(&mut out, optional + 56, size);
    put32(&mut out, optional + 60, 0x400);
    for (index, &(virtual_size, address, raw_size, raw_at, flags)) in sections.iter().enumerate() {
        let at = table + 40 * index;
        put32(&mut out, at + 8, virtual_size);
        put32(&mut out, at + 12, address);
        put32(&mut out, at + 16, raw_size);
        put32(&mut out, at + 20, raw_at);
        put32(&mut out, at + 36, flags);
    }
    out
}

const CODE: u32 = 0x6000_0020;
const RDATA: u32 = 0x4000_0040;
const DATA: u32 = 0xc000_0040;

fn a_good_pe() -> Vec<u8> {
    pe(
        &[
            (0x8000, 0x1_0000, 0x8000, 0x1_0000, CODE),
            (0x123, 0x1_8000, 0x200, 0x1_8000, CODE),
            (0x5000, 0x2_0000, 0x5000, 0x2_0000, RDATA),
            (0x800, 0x2_5000, 0x200, 0x2_5000, DATA),
            (0x20_0000, 0x3_0000, 0, 0, 0),
        ],
        0x23_0000,
    )
}

#[test]
fn a_pe_gives_its_code_as_one_segment_on_granules() {
    let got = layout(Format::Pe, &a_good_pe(), GRANULE).unwrap();
    assert_eq!(got, Layout {
        format: Format::Pe,
        header: BASE,
        span: BASE..BASE + 0x23_0000,
        code: vec![Segment { offset: 0x1_0000, linked: BASE + 0x1_0000, len: 0x1_0000 }],
        reserved: vec![BASE + 0x3_0000..BASE + 0x23_0000],
    });
    assert_eq!(headers_len(Format::Pe, &a_good_pe()), Some(0x400));
}

#[test]
fn a_pe_is_refused_for_what_a_run_cannot_overlay() {
    let refused = |sections: &[(u32, u32, u32, u32, u32)], says: &str| {
        let error = layout(Format::Pe, &pe(sections, 0x10_0000), GRANULE).unwrap_err();
        assert!(error.what.contains(says), "{error} does not say {says:?}");
    };
    refused(&[(0x100, 0x1000, 0x200, 0x1000, CODE)], "does not start on a page");
    refused(&[(0x100, 0x1_0000, 0x200, 0x1_0000, CODE | 0x8000_0000)], "writable and executable");
    refused(&[(0x100, 0x1_0000, 0x200, 0x400, CODE)], "raw data at");
    refused(&[(0x300, 0x1_0000, 0x200, 0x1_0000, CODE)], "all raw data");
    refused(&[(0x100, 0x1_0000, 0x200, 0x1_0000, CODE), (0x100, 0x1_1000, 0x200, 0x1_1000, DATA)], "shares a page");
    refused(&[(0x100, 0x1_0000, 0x200, 0x1_0000, CODE), (0x1000, 0x2_1000, 0, 0, 0)], "not whole pages");
    let mut dll = a_good_pe();
    put16(&mut dll, 0x80 + 22, 0x2022);
    assert!(layout(Format::Pe, &dll, GRANULE).unwrap_err().what.contains("not an executable"));
}

#[test]
fn headers_mutated_or_cut_are_an_error_or_a_layout_never_a_panic() {
    let cases = [(Format::Elf, a_good_elf(), PAGE), (Format::MachO, a_good_macho(), MPAGE), (Format::Pe, a_good_pe(), GRANULE)];
    let mut rng = SplitMix64::new(0x1a40_77ee_0000_0003);
    for (format, bytes, page) in cases {
        for cut in 0..bytes.len() {
            let _ = layout(format, &bytes[..cut], page);
            let _ = headers_len(format, &bytes[..cut]);
        }
        for _ in 0..20_000 {
            let mut mutated = bytes.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.index(mutated.len());
                mutated[at] = rng.next_u64() as u8;
            }
            if let Ok(got) = layout(format, &mutated, page) {
                for segment in &got.code {
                    assert!(segment.linked.is_multiple_of(page) && segment.len.is_multiple_of(page) && segment.len != 0);
                    assert!(segment.linked >= got.span.start && segment.linked + segment.len <= got.span.end);
                }
                for range in &got.reserved {
                    assert!(range.start.is_multiple_of(page) && range.end.is_multiple_of(page) && range.start < range.end);
                    assert!(got.code.iter().all(|it| it.linked + it.len <= range.start || range.end <= it.linked));
                }
            }
        }
    }
    assert!(layout(Format::Elf, &a_good_elf(), 3).is_err());
}
