//! Executables' headers built by hand, for the image tests here and blaze-inproc's (the
//! `testing` feature, which only crates/blaze-inproc-rt's build turns on).

pub const PAGE: u64 = 0x1000;

pub fn put16(out: &mut Vec<u8>, at: usize, value: u16) {
    out[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn put32(out: &mut Vec<u8>, at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put64(out: &mut Vec<u8>, at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// `PT_LOAD`s of (flags, offset, vaddr, filesz, memsz).
pub fn elf(loads: &[(u32, u64, u64, u64, u64)]) -> Vec<u8> {
    let mut out = vec![0u8; 64 + 56 * (loads.len() + 1)];
    out[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
    put16(&mut out, 16, 3);
    put16(&mut out, 18, super::elf::MACHINE);
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

pub const R: u32 = 4;
pub const RX: u32 = 5;
