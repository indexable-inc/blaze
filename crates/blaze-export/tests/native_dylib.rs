//! W4 (NATIVE.md 3.1, 3.4, 3.5): the native library record written as a Mach-O `MH_DYLIB` around
//! a hand-laid `blaze_native::image::Image`, read back by a reader here (no dlopen: W11 loads it).
//!
//! Formats quoted from the SDK headers: `<mach-o/loader.h>` (`mach_header_64`: magic
//! 0xfeedfacf, `filetype` `MH_DYLIB` 6; `dylib_command`: `name.offset` from the command's start;
//! `LC_ID_DYLIB` 0xd, `LC_LOAD_DYLIB` 0xc, `LC_DYLD_CHAINED_FIXUPS` 0x80000034,
//! `LC_DYLD_EXPORTS_TRIE` 0x80000033; the export trie: per node a uleb `terminal_size`, then
//! uleb flags and address when terminal, a child count byte, per child a NUL-terminated label and
//! a uleb offset), `<mach-o/fixup-chains.h>` (`dyld_chained_fixups_header`: fixups_version,
//! starts_offset, imports_offset, symbols_offset, imports_count, imports_format, symbols_format;
//! `DYLD_CHAINED_IMPORT_ADDEND64` 3: `lib_ordinal:16, weak_import:1, reserved:15,
//! name_offset:32; addend:64`; `dyld_chained_ptr_64_bind`: `ordinal:24, addend:8, reserved:19,
//! next:12, bind:1`; `dyld_chained_ptr_64_rebase`: `target:36, high8:8, reserved:7, next:12,
//! bind:0`, next in 4-byte strides), `<mach-o/loader.h>`'s `BIND_SPECIAL_DYLIB_FLAT_LOOKUP` -2.
//!
//! Without the piece: "unresolved import `blaze_export::native_dylib`".

use blaze_export::native_dylib::{FLAT_LOOKUP, write};
use blaze_native::Platform;
use blaze_native::image::{FixTo, Fixup, Image, Import, Placed, Region};
use blaze_native::libraries::{Library, LibraryKind, Linked, Source, source};
use blaze_own::{AVec, Alloc, Vec};

const IDENT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const START: u64 = 0x4000;
const RET: u32 = 0xd65f_03c0;

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `-l<name>` linked as the SDK's stub states `install` (blaze-native libraries.rs: a macOS
/// library's install name is its stub's, read).
fn mac_dylib(name: &'static str, install: &'static str) -> Linked<'static> {
    let Ok(Source::Stub(at)) = source(Platform::MacosArm64, Library { kind: LibraryKind::Dylib, name }) else { panic!("`{name}` is named by its stub") };
    let stub = format!("--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '{install}'\n...\n");
    at.linked(Box::leak(stub.into_bytes().into_boxed_slice())).unwrap_or_else(|e| panic!("the stub of `{name}`: {e}"))
}

fn put32(bytes: &mut [u8], at: u64, value: u32) {
    let i = (at - START) as usize;
    bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], at: u64, value: u64) {
    let i = (at - START) as usize;
    bytes[i..i + 8].copy_from_slice(&value.to_le_bytes());
}

/// Two C functions (`_c_entry` at 0x4000, `_c_entry2` at 0x4004), one GOT cell bound to
/// `_puts`, the descriptor, a data pointer to `_c_entry`, a data pointer to `_weak_hook + 8`,
/// one Rust word, one initializer (`_c_entry2`), 16 bytes of zero fill; libraries c, z, m.
fn image(word_pointer: bool) -> Image<'static> {
    let a = alloc();
    let mut regions: AVec<'static, Placed> = Vec::new_in(a);
    let rows: [(Region, u64, u64, u8); 12] = [
        (Region::Text, 0x4000, 8, 2),
        (Region::Const, 0x4008, 0, 0),
        (Region::EhFrame, 0x4008, 0, 3),
        (Region::UnwindInfo, 0x4008, 0, 2),
        (Region::Got, 0x8000, 8, 3),
        (Region::Data, 0xC000, 48, 3),
        (Region::Words, 0xC030, 8, 3),
        (Region::Inits, 0xC038, 8, 3),
        (Region::ThreadVars, 0xC040, 0, 3),
        (Region::ThreadData, 0xC040, 0, 0),
        (Region::ThreadZero, 0xC040, 0, 0),
        (Region::Zero, 0xC040, 16, 3),
    ];
    for (region, at, len, align_log2) in rows {
        regions.push(Placed { region, at, len, align_log2 });
    }
    let mut bytes: AVec<'static, u8> = Vec::new_in(a);
    bytes.resize((0xC040 - START) as usize, 0);
    put32(&mut bytes, 0x4000, RET);
    put32(&mut bytes, 0x4004, RET);
    // The descriptor (NATIVE.md 3.4): "BNL1", one word, one init, pad, word_at, init_at.
    put32(&mut bytes, 0xC000, 0x424e_4c31);
    put32(&mut bytes, 0xC004, 1);
    put32(&mut bytes, 0xC008, 1);
    put64(&mut bytes, 0xC010, 0xC030);
    put64(&mut bytes, 0xC018, 0xC038);
    let mut fixups: AVec<'static, Fixup> = Vec::new_in(a);
    fixups.push(Fixup { at: 0x8000, to: FixTo::Import { index: 0, addend: 0 } });
    let first = if word_pointer { FixTo::Word { index: 0 } } else { FixTo::Rebase { target: 0x4000 } };
    fixups.push(Fixup { at: 0xC020, to: first });
    fixups.push(Fixup { at: 0xC028, to: FixTo::Import { index: 1, addend: 8 } });
    fixups.push(Fixup { at: 0xC038, to: FixTo::Rebase { target: 0x4004 } });
    let mut imports: AVec<'static, Import<'static>> = Vec::new_in(a);
    imports.push(Import { name: b"_puts", weak: false });
    imports.push(Import { name: b"_weak_hook", weak: true });
    let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(a);
    exports.push((b"___blaze_native_v1", 0xC000));
    exports.push((b"_c_entry", 0x4000));
    exports.push((b"_c_entry2", 0x4004));
    let mut words: AVec<'static, &'static [u8]> = Vec::new_in(a);
    words.push(b"_rust_six");
    let mut libraries: AVec<'static, Linked<'static>> = Vec::new_in(a);
    libraries.push(mac_dylib("c", "/usr/lib/libSystem.B.dylib"));
    libraries.push(mac_dylib("z", "/usr/lib/libz.1.dylib"));
    libraries.push(mac_dylib("m", "/usr/lib/libSystem.B.dylib"));
    Image {
        platform: Platform::MacosArm64,
        regions,
        bytes,
        file_start: START,
        end: 0xC050,
        fixups,
        tls: Vec::new_in(a),
        imports,
        exports,
        words,
        inits: 1,
        descriptor_at: 0xC000,
        libraries,
        unwind: Vec::new_in(a),
        frames: Vec::new_in(a),
        eh_pointers: Vec::new_in(a),
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from(u32_at(b, at)) | (u64::from(u32_at(b, at + 4)) << 32)
}

fn cstr(b: &[u8], at: usize) -> &[u8] {
    let len = b[at..].iter().position(|&c| c == 0).expect("a NUL-terminated string");
    &b[at..at + len]
}

fn uleb(b: &[u8], at: &mut usize) -> u64 {
    let (mut value, mut shift) = (0u64, 0);
    loop {
        let byte = b[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

/// The address the export trie gives `name`, walked node by node.
fn trie_lookup(trie: &[u8], name: &[u8]) -> Option<u64> {
    let (mut node, mut rest) = (0usize, name);
    loop {
        let mut p = node;
        let terminal = uleb(trie, &mut p);
        if rest.is_empty() {
            if terminal == 0 {
                return None;
            }
            let _flags = uleb(trie, &mut p);
            return Some(uleb(trie, &mut p));
        }
        p += terminal as usize;
        let count = trie[p];
        p += 1;
        let mut next = None;
        for _ in 0..count {
            let label = cstr(trie, p);
            p += label.len() + 1;
            let offset = uleb(trie, &mut p) as usize;
            if next.is_none() && rest.starts_with(label) {
                next = Some((offset, label.len()));
            }
        }
        let (offset, len) = next?;
        node = offset;
        rest = &rest[len..];
    }
}

fn written(image: &Image<'static>) -> AVec<'static, u8> {
    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    match write(image, IDENT, &mut out, alloc()) {
        Ok(()) => out,
        Err(e) => panic!("write refused: {e:?}"),
    }
}

/// The written file is a dylib named by the identifier, loading libSystem then libz (c and m
/// are libSystem: one load command), every import row by flat lookup with its weak bit and
/// addend, each slot chained, and the descriptor and both C functions in the export trie.
/// Prints: `native_dylib: MH_DYLIB <N> bytes, id <64 hex>, loads libSystem and libz, 3 imports by flat lookup (0xfffe), ___blaze_native_v1 at 0xc000`
#[test]
fn writes_loadable_shape() {
    let file = written(&image(false));
    assert_eq!(u32_at(&file, 0), 0xfeed_facf, "mach_header_64 magic");
    assert_eq!(u32_at(&file, 12), 6, "filetype MH_DYLIB");
    let ncmds = u32_at(&file, 16);
    let (mut id, mut loads) = (None, Vec::new_in(alloc()));
    let (mut fixups, mut trie) = (None, None);
    let mut at = 32usize;
    for _ in 0..ncmds {
        let (cmd, size) = (u32_at(&file, at), u32_at(&file, at + 4) as usize);
        match cmd {
            0xd => id = Some(cstr(&file, at + u32_at(&file, at + 8) as usize)),
            0xc => loads.push(cstr(&file, at + u32_at(&file, at + 8) as usize)),
            0x8000_0034 => fixups = Some((u32_at(&file, at + 8) as usize, u32_at(&file, at + 12) as usize)),
            0x8000_0033 => trie = Some((u32_at(&file, at + 8) as usize, u32_at(&file, at + 12) as usize)),
            _ => {}
        }
        at += size;
    }
    assert_eq!(id, Some(IDENT.as_bytes()), "LC_ID_DYLIB is the identifier");
    let want: [&[u8]; 2] = [b"/usr/lib/libSystem.B.dylib", b"/usr/lib/libz.1.dylib"];
    assert_eq!(&loads[..], &want[..], "libSystem first, each install name once");

    let (f, _) = fixups.expect("LC_DYLD_CHAINED_FIXUPS");
    assert_eq!(u32_at(&file, f + 20), 3, "imports_format DYLD_CHAINED_IMPORT_ADDEND64");
    let count = u32_at(&file, f + 16) as usize;
    assert_eq!(count, 3, "two imports, and _weak_hook + 8 in a row of its own");
    let (rows, symbols) = (f + u32_at(&file, f + 8) as usize, f + u32_at(&file, f + 12) as usize);
    let want_rows: [(&[u8], u64, u64); 3] = [(b"_puts", 0, 0), (b"_weak_hook", 1, 0), (b"_weak_hook", 1, 8)];
    for (k, (name, weak, addend)) in want_rows.into_iter().enumerate() {
        let row = u64_at(&file, rows + 16 * k);
        assert_eq!((row & 0xffff) as u16, FLAT_LOOKUP, "row {k}: lib_ordinal is BIND_SPECIAL_DYLIB_FLAT_LOOKUP");
        assert_eq!((row & 0xffff) as u16 as i16, -2, "row {k}: -2 in the 16-bit field");
        assert_eq!((row >> 16) & 1, weak, "row {k}: weak_import");
        assert_eq!(cstr(&file, symbols + (row >> 32) as usize), name, "row {k}: name");
        assert_eq!(u64_at(&file, rows + 16 * k + 8), addend, "row {k}: addend");
    }
    assert_eq!(u64_at(&file, 0x8000), 1 << 63, "the GOT cell binds row 0, last on its page");
    assert_eq!(u64_at(&file, 0xC020), 0x4000 | (2 << 51), "a rebase to 0x4000, next slot 8 bytes on");
    assert_eq!(u64_at(&file, 0xC028), 2 | (4 << 51) | (1 << 63), "binds row 2 (the addend's), next 16 bytes on");
    assert_eq!(u64_at(&file, 0xC038), 0x4004, "the initializer: a rebase to 0x4004, last");
    assert_eq!(u32_at(&file, 0xC000), 0x424e_4c31, "the descriptor kept at its address");

    let (t, len) = trie.expect("LC_DYLD_EXPORTS_TRIE");
    let trie = &file[t..t + len];
    assert_eq!(trie_lookup(trie, b"___blaze_native_v1"), Some(0xC000));
    assert_eq!(trie_lookup(trie, b"_c_entry"), Some(0x4000));
    assert_eq!(trie_lookup(trie, b"_c_entry2"), Some(0x4004));
    assert_eq!(trie_lookup(trie, b"_rust_six"), None, "a Rust word is not exported");
    println!(
        "native_dylib: MH_DYLIB {} bytes, id {IDENT}, loads libSystem and libz, {count} imports by flat lookup ({FLAT_LOOKUP:#x}), ___blaze_native_v1 at {:#x}",
        file.len(),
        trie_lookup(trie, b"___blaze_native_v1").unwrap_or(0)
    );
}

/// NATIVE.md 3.10: the bytes are a function of the image and the identifier.
/// Prints: `native_dylib: two writes, <N> equal bytes`
#[test]
fn same_bytes_twice() {
    let (one, two) = (written(&image(false)), written(&image(false)));
    assert!(one[..] == two[..], "two writes of one image differ");
    println!("native_dylib: two writes, {} equal bytes", one.len());
}

/// A C data word holding a Rust function's address is the launcher's to write from the
/// descriptor's pointer rows (BNL2): the writer emits no dynamic fixup for it, so its file bytes
/// stay 0. The control: the same cell as a rebase holds its chained-fixup entry (non-zero), so
/// a cell put into a chain would show. File offset equals address in data (module doc).
/// Prints: `native_dylib: word pointer at 0xc020 left to the launcher (cell 0; the same cell as a rebase holds 0x...)`
#[test]
fn word_pointer_left_to_launcher() {
    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    write(&image(true), IDENT, &mut out, alloc()).expect("a word pointer is left to the launcher, not refused");
    let cell = u64::from_le_bytes(out[0xC020..0xC028].try_into().expect("8 bytes"));
    assert_eq!(cell, 0, "the launcher's cell is in no chain");
    let mut rebased: AVec<'static, u8> = Vec::new_in(alloc());
    write(&image(false), IDENT, &mut rebased, alloc()).expect("the same image with a rebase there");
    let chained = u64::from_le_bytes(rebased[0xC020..0xC028].try_into().expect("8 bytes"));
    assert_ne!(chained, 0, "control: a rebase slot holds its chain entry");
    println!("native_dylib: word pointer at 0xc020 left to the launcher (cell 0; the same cell as a rebase holds {chained:#x})");
}
