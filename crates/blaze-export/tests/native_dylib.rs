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
    let first = if word_pointer { FixTo::Word { index: 0 } } else { FixTo::rebase(0x4000) };
    fixups.push(Fixup { at: 0xC020, to: first });
    fixups.push(Fixup { at: 0xC028, to: FixTo::Import { index: 1, addend: 8 } });
    fixups.push(Fixup { at: 0xC038, to: FixTo::rebase(0x4004) });
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

/// dyld reads `ncmds` load commands and no more: the header's count covers every command up to
/// `sizeofcmds`, and among them is the `LC_CODE_SIGNATURE` whose blob ends the file, a super
/// blob whose code directory's limit is the blob's offset. The cause it guards: a hand count one
/// short left LC_CODE_SIGNATURE (the last command) unread, and every image was refused at
/// `dlopen` as "missing code signature". Control: the count one less no longer reaches it.
#[test]
fn every_command_is_counted_and_the_signature_is_read() {
    let file = written(&image(false));
    let (ncmds, sizeofcmds) = (u32_at(&file, 16), u32_at(&file, 20) as usize);
    let walk = |count: u32| {
        let (mut at, mut signature) = (32usize, None);
        for _ in 0..count {
            let (cmd, size) = (u32_at(&file, at), u32_at(&file, at + 4) as usize);
            if cmd == 0x1d {
                signature = Some((u32_at(&file, at + 8) as usize, u32_at(&file, at + 12) as usize));
            }
            at += size;
        }
        (at, signature)
    };
    let (end, signature) = walk(ncmds);
    assert_eq!(end, 32 + sizeofcmds, "ncmds commands end exactly at sizeofcmds");
    let (off, size) = signature.expect("LC_CODE_SIGNATURE among the counted commands");
    assert_eq!(off + size, file.len(), "the signature ends the file");
    let be = |at: usize| u32::from_be_bytes(file[at..at + 4].try_into().expect("four bytes"));
    assert_eq!(be(off), 0xFADE_0CC0, "CSMAGIC_EMBEDDED_SIGNATURE at the command's offset");
    let directory = off + be(off + 16) as usize;
    assert_eq!(be(directory), 0xFADE_0C02, "the code directory the blob's index names");
    assert_eq!(be(directory + 32) as usize, off, "codeLimit: every byte before the signature");
    assert_eq!(walk(ncmds - 1).1, None, "control: one command fewer does not reach the signature");
}

/// The load commands are a list built first and counted from it (commands.rs), so a dylib with
/// many `LC_LOAD_DYLIB`s of 300-byte install names lays out and every command reads back by
/// walking the header: `ncmds` of them end exactly at `sizeofcmds`, each `cmdsize` a multiple of
/// 8, the install names in order, the signature counted. Twin: ten more names pass the 0x4000
/// the image keeps before `Text` and are refused by type, in every build, with the need the
/// list's own size (each added `dylib_command` 24 + 304 bytes).
#[test]
fn long_install_names_read_back_by_walking_and_past_the_room_is_refused_by_type() {
    use blaze_export::native_dylib::DylibError;
    let long = |k: usize| -> &'static str { Box::leak(format!("/opt/blaze-room/{k:03}/{}", "n".repeat(280)).into_boxed_str()) };
    let with = |count: usize| {
        let mut img = image(false);
        for k in 0..count {
            img.libraries.push(mac_dylib("z", long(k)));
        }
        img
    };
    assert_eq!(long(0).len(), 300, "a 300-byte install name");
    let fits = 40;
    let file = written(&with(fits));
    let (ncmds, sizeofcmds) = (u32_at(&file, 16), u32_at(&file, 20) as usize);
    let (mut at, mut loads, mut signature) = (32usize, Vec::new_in(alloc()), None);
    for _ in 0..ncmds {
        let (cmd, size) = (u32_at(&file, at), u32_at(&file, at + 4) as usize);
        assert!(size > 0 && size.is_multiple_of(8), "cmdsize {size} at {at:#x}");
        if cmd == 0xc {
            loads.push(cstr(&file, at + u32_at(&file, at + 8) as usize));
        }
        if cmd == 0x1d {
            signature = Some(u32_at(&file, at + 8) as usize);
        }
        at += size;
    }
    assert_eq!(at, 32 + sizeofcmds, "ncmds commands end exactly at sizeofcmds");
    assert!(at <= 0x4000, "the commands fit the image's header room");
    assert_eq!(loads.len(), 2 + fits, "libSystem, libz, then each long name once");
    assert_eq!((loads[0], loads[1]), (&b"/usr/lib/libSystem.B.dylib"[..], &b"/usr/lib/libz.1.dylib"[..]));
    for k in 0..fits {
        assert_eq!(loads[2 + k], long(k).as_bytes(), "LC_LOAD_DYLIB {k} reads back whole");
    }
    assert!(signature.is_some(), "LC_CODE_SIGNATURE is among the counted commands");
    // Twin: ten more do not fit the room; refused by type, the need the list's own size.
    let too_many = fits + 10;
    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    match write(&with(too_many), IDENT, &mut out, alloc()) {
        Err(DylibError::HeaderRoom { need, room }) => {
            assert_eq!(room, 0x4000, "the room is where Text starts");
            assert_eq!(need as usize, 32 + sizeofcmds + 10 * (24 + 304), "the need is the list's size");
        }
        other => panic!("{too_many} long install names were not refused by the header room: {other:?}"),
    }
}

/// `dyld_chained_ptr_64_rebase` keeps a target's top byte in `high8` (bits 36..43; dyld:
/// header + (high8 << 56 | target)): Apple arm64 non-unique RTTI's typeinfo name pointer, bit
/// 63 over an image place (tempo's rocksdb), is written so, with its 36-bit target. Bits
/// 36..55 are no image place: refused naming the slot's address (rule 19). Control: the
/// untagged rebase has high8 0.
#[test]
fn a_rebase_keeps_its_top_byte_and_refuses_bits_36_to_55_by_slot() {
    use blaze_export::native_dylib::DylibError;
    let mut tagged = image(false);
    tagged.fixups[1] = Fixup { at: 0xC020, to: FixTo::rebase((1 << 63) | 0x4000) };
    assert!(matches!(tagged.fixups[1].to, FixTo::Rebase { address, high8: 0x80 } if address.get() == 0x4000), "the tag is its own field");
    let file = written(&tagged);
    // The next slot is 0xC028, 8 bytes (two 4-byte strides) on.
    assert_eq!(u64_at(&file, 0xC020), 0x4000 | (0x80 << 36) | (2 << 51), "high8 0x80, target 0x4000");
    assert_eq!(u64_at(&file, 0xC038), 0x4004, "control: the untagged rebase has high8 0");
    for target in [(1u64 << 55) | 0x4000, 1 << 40] {
        let mut far = image(false);
        far.fixups[1] = Fixup { at: 0xC020, to: FixTo::rebase(target) };
        let mut out: AVec<'static, u8> = Vec::new_in(alloc());
        match write(&far, IDENT, &mut out, alloc()) {
            Err(DylibError::Slot { at, value, .. }) => assert_eq!((at, value), (0xC020, target)),
            other => panic!("the rebase target {target:#x} was not refused by its slot: {other:?}"),
        }
    }
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
