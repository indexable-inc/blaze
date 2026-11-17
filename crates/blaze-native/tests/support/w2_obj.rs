//! W2's in-test byte builder: a minimal arm64 Mach-O relocatable object (one `__TEXT,__text`
//! section, an `LC_SYMTAB`, no relocations) and a GNU `ar` archive of such objects. No external
//! tool. (W1's `tests/support/obj.rs` was not written when this was, and its interface is not
//! fixed by NATIVE.md; this builder is W2's own and may be folded into it later.)
//!
//! The format's rules, from the SDK's <mach-o/loader.h> and <mach-o/nlist.h>: `MH_MAGIC_64`
//! 0xfeedfacf, `CPU_TYPE_ARM64` 0x0100000c, `MH_OBJECT` 1, `LC_SEGMENT_64` 0x19 (72 bytes and 80
//! per section), `LC_SYMTAB` 2 (24 bytes), `nlist_64` 16 bytes; `N_EXT` 0x01, `N_SECT` 0x0e,
//! `N_UNDF` 0; "The N_WEAK_REF bit of the n_desc field indicates to the dynamic linker that the
//! undefined symbol is allowed to be missing" (`N_WEAK_REF` 0x0040); "The N_WEAK_DEF bit of the
//! n_desc field indicates to the static and dynamic linkers that the symbol definition is weak"
//! (`N_WEAK_DEF` 0x0080); "Common symbols are represented by undefined (N_UNDF) external (N_EXT)
//! types who's values (n_value) are non-zero ... the n_value field is the size", the alignment's
//! log2 in bits 8..12 of `n_desc` (`SET_COMM_ALIGN`).

use blaze_own::{AVec, Alloc};

/// One global symbol of a built object.
#[derive(Clone, Copy)]
pub enum Sym<'s> {
    Def(&'s str),
    WeakDef(&'s str),
    Undef(&'s str),
    WeakUndef(&'s str),
    Common(&'s str, u64, u8),
}

const N_EXT: u8 = 0x01;
const N_SECT: u8 = 0x0e;
const N_WEAK_REF: u16 = 0x0040;
const N_WEAK_DEF: u16 = 0x0080;
const RET: u32 = 0xd65f_03c0;

fn u16le(out: &mut AVec<'static, u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn u32le(out: &mut AVec<'static, u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn u64le(out: &mut AVec<'static, u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn name16(out: &mut AVec<'static, u8>, name: &str) {
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name.as_bytes());
    out.extend_from_slice(&field);
}
fn align8(n: u32) -> u32 {
    (n + 7) & !7
}

fn sym_name<'s>(sym: Sym<'s>) -> &'s str {
    match sym {
        Sym::Def(n) | Sym::WeakDef(n) | Sym::Undef(n) | Sym::WeakUndef(n) | Sym::Common(n, _, _) => n,
    }
}

/// A Mach-O arm64 `MH_OBJECT` whose symbols are `symbols` (names given mangled, `_f`); each
/// definition is a `ret` of its own in `__TEXT,__text`.
pub fn macho(symbols: &[Sym<'_>]) -> AVec<'static, u8> {
    let alloc = Alloc::global();
    let defs = symbols.iter().filter(|s| matches!(s, Sym::Def(_) | Sym::WeakDef(_))).count() as u32;
    let text_size = 4 * defs.max(1);
    let text_off = 32 + 152 + 24;
    let sym_off = align8(text_off + text_size);
    let nsyms = symbols.len() as u32;
    let str_off = sym_off + 16 * nsyms;

    let mut strings = AVec::new_in(alloc);
    strings.push(0u8);
    let mut strx = AVec::new_in(alloc);
    for &sym in symbols {
        strx.push(strings.len() as u32);
        strings.extend_from_slice(sym_name(sym).as_bytes());
        strings.push(0u8);
    }
    while strings.len() % 8 != 0 {
        strings.push(0u8);
    }

    let mut out = AVec::new_in(alloc);
    // mach_header_64
    u32le(&mut out, 0xfeed_facf);
    u32le(&mut out, 0x0100_000c);
    u32le(&mut out, 0);
    u32le(&mut out, 1);
    u32le(&mut out, 2);
    u32le(&mut out, 152 + 24);
    u32le(&mut out, 0);
    u32le(&mut out, 0);
    // LC_SEGMENT_64 with one section
    u32le(&mut out, 0x19);
    u32le(&mut out, 152);
    name16(&mut out, "");
    u64le(&mut out, 0);
    u64le(&mut out, u64::from(text_size));
    u64le(&mut out, u64::from(text_off));
    u64le(&mut out, u64::from(text_size));
    u32le(&mut out, 7);
    u32le(&mut out, 7);
    u32le(&mut out, 1);
    u32le(&mut out, 0);
    // section_64 __TEXT,__text
    name16(&mut out, "__text");
    name16(&mut out, "__TEXT");
    u64le(&mut out, 0);
    u64le(&mut out, u64::from(text_size));
    u32le(&mut out, text_off);
    u32le(&mut out, 2);
    u32le(&mut out, 0);
    u32le(&mut out, 0);
    u32le(&mut out, 0x8000_0400);
    u32le(&mut out, 0);
    u32le(&mut out, 0);
    u32le(&mut out, 0);
    // LC_SYMTAB
    u32le(&mut out, 2);
    u32le(&mut out, 24);
    u32le(&mut out, sym_off);
    u32le(&mut out, nsyms);
    u32le(&mut out, str_off);
    u32le(&mut out, strings.len() as u32);
    // __text
    for _ in 0..text_size / 4 {
        u32le(&mut out, RET);
    }
    while (out.len() as u32) < sym_off {
        out.push(0);
    }
    // nlist_64
    let mut next_def = 0u64;
    for (i, &sym) in symbols.iter().enumerate() {
        let (n_type, n_sect, n_desc, n_value) = match sym {
            Sym::Def(_) | Sym::WeakDef(_) => {
                let value = 4 * next_def;
                next_def += 1;
                let desc = if matches!(sym, Sym::WeakDef(_)) { N_WEAK_DEF } else { 0 };
                (N_SECT | N_EXT, 1u8, desc, value)
            }
            Sym::Undef(_) => (N_EXT, 0, 0, 0),
            Sym::WeakUndef(_) => (N_EXT, 0, N_WEAK_REF, 0),
            Sym::Common(_, size, align_log2) => (N_EXT, 0, (u16::from(align_log2) & 0x0f) << 8, size),
        };
        u32le(&mut out, strx[i]);
        out.push(n_type);
        out.push(n_sect);
        u16le(&mut out, n_desc);
        u64le(&mut out, n_value);
    }
    out.extend_from_slice(&strings);
    out
}

/// A GNU `ar` archive (`!<arch>\n`, 60-byte headers, short names ending in `/`) of `members`.
pub fn archive(members: &[(&str, &[u8])]) -> AVec<'static, u8> {
    let mut out = AVec::new_in(Alloc::global());
    out.extend_from_slice(b"!<arch>\n");
    for &(name, bytes) in members {
        let mut header = [b' '; 60];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[name.len()] = b'/';
        header[16] = b'0'; // mtime
        header[28] = b'0'; // uid
        header[34] = b'0'; // gid
        header[40..43].copy_from_slice(b"644"); // mode
        let mut size = [0u8; 20];
        let mut n = bytes.len();
        let mut digits = 0;
        loop {
            size[digits] = b'0' + (n % 10) as u8;
            digits += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for k in 0..digits {
            header[48 + k] = size[digits - 1 - k];
        }
        header[58] = b'`';
        header[59] = b'\n';
        out.extend_from_slice(&header);
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}
