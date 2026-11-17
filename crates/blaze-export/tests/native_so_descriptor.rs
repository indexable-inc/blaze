//! The BNL2 descriptor round trip on Linux (NATIVE.md 3.4 and W5): blaze-export's ELF shared
//! object writer (native_so.rs) around a hand-laid image, then blaze-native's descriptor reader
//! (descriptor.rs `read` with `Platform::LinuxArm64`, its ELF half descriptor/so.rs), gives the
//! descriptor and the names the image holds, byte for byte. Two controls: `.data`'s `sh_size` cut
//! below 48 bytes, and the file cut before its section headers, are each refused by name.
//!
//! Formats from the System V gABI: `Elf64_Ehdr.e_shoff` at 40, `e_shnum` at 60; `Elf64_Shdr` 64
//! bytes, `sh_type` at 4 (`SHT_PROGBITS` 1), `sh_addr` at 16, `sh_size` at 32.
//!
//! Without the piece: `read` returns `DescriptorError::ElfNotBuilt` and the test fails at "the
//! Linux record's descriptor reads"; with the reader's `.data` lookup broken, at `NoData`.

use blaze_export::native_so::write;
use blaze_native::Platform;
use blaze_native::descriptor::{self, Descriptor, DescriptorError, MAGIC};
use blaze_native::image::{Image, Placed, Region};
use blaze_own::{AVec, Alloc, Vec};

const SONAME: &str = "libdescriptor-round-trip.so";
const START: u64 = 0x1_0000;
const DATA: u64 = 0x2_0000;
const RET: u32 = 0xd65f_03c0;

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn put32(bytes: &mut [u8], at: u64, value: u32) {
    let i = (at - START) as usize;
    bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
}

/// One `ret` at 0x10000; Data at 0x20000: the BNL2 descriptor (two words, no inits, no pointer
/// rows), the names table right after it (rows {16, 2} and {18, 3} from `name_at` 0x20030, then
/// "abcde"); the two word cells at 0x20048. No Got, so Data starts the page after the text.
fn image(descriptor: &Descriptor) -> Image<'static> {
    let a = alloc();
    let mut regions: AVec<'static, Placed> = Vec::new_in(a);
    let rows: [(Region, u64, u64, u8); 12] = [
        (Region::Text, 0x1_0000, 4, 2),
        (Region::Const, 0x1_0004, 0, 0),
        (Region::EhFrame, 0x1_0004, 0, 3),
        (Region::UnwindInfo, 0x1_0004, 0, 2),
        (Region::Got, 0x2_0000, 0, 3),
        (Region::Data, 0x2_0000, 0x48, 3),
        (Region::Words, 0x2_0048, 0x10, 3),
        (Region::Inits, 0x2_0058, 0, 3),
        (Region::ThreadVars, 0x2_0058, 0, 3),
        (Region::ThreadData, 0x2_0058, 0, 3),
        (Region::ThreadZero, 0x2_0058, 0, 3),
        (Region::Zero, 0x2_0058, 0, 3),
    ];
    for (region, at, len, align_log2) in rows {
        regions.push(Placed { region, at, len, align_log2 });
    }
    let mut bytes: AVec<'static, u8> = Vec::new_in(a);
    bytes.resize((0x2_0058 - START) as usize, 0);
    put32(&mut bytes, 0x1_0000, RET);
    let d = (DATA - START) as usize;
    bytes[d..d + 48].copy_from_slice(&descriptor.to_bytes());
    put32(&mut bytes, 0x2_0030, 16);
    put32(&mut bytes, 0x2_0034, 2);
    put32(&mut bytes, 0x2_0038, 18);
    put32(&mut bytes, 0x2_003C, 3);
    let n = (0x2_0040 - START) as usize;
    bytes[n..n + 5].copy_from_slice(b"abcde");
    let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(a);
    exports.push((b"__blaze_native_v1", DATA));
    let mut words: AVec<'static, &'static [u8]> = Vec::new_in(a);
    words.push(b"ab");
    words.push(b"cde");
    Image {
        platform: Platform::LinuxArm64,
        regions,
        bytes,
        file_start: START,
        end: 0x2_0058,
        fixups: Vec::new_in(a),
        tls: Vec::new_in(a),
        imports: Vec::new_in(a),
        exports,
        words,
        inits: 0,
        descriptor_at: DATA,
        libraries: Vec::new_in(a),
        unwind: Vec::new_in(a),
        frames: Vec::new_in(a),
        eh_pointers: Vec::new_in(a),
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// W5's writer, then the Linux reader: the same descriptor and names as the image; the two
/// truncation controls refused naming what was cut. Prints "native_so_descriptor: BNL2 read back
/// from W5's .so equal to the image's (2 names [ab, cde]); .data cut to 40 bytes refused as
/// Truncated; file cut before the section headers refused as Truncated".
#[test]
fn descriptor_round_trips_through_w5() {
    let want = Descriptor {
        magic: MAGIC,
        words: 2,
        inits: 0,
        pointers: 0,
        word_at: 0x2_0048,
        init_at: 0x2_0058,
        pointer_at: 0x2_0048,
        name_at: 0x2_0030,
    };
    let img = image(&want);
    let mut so: AVec<'static, u8> = Vec::new_in(alloc());
    write(&img, SONAME, &mut so, alloc()).expect("W5 writes the image");

    let (got, names) = descriptor::read(&so, Platform::LinuxArm64).expect("the Linux record's descriptor reads");
    assert_eq!(got, want);
    let d = (DATA - START) as usize;
    let input: &[u8; 48] = img.bytes[d..d + 48].try_into().expect("48 bytes");
    assert_eq!(got, Descriptor::from_bytes(input));
    assert_eq!(names.len(), img.words.len());
    for (i, &word) in img.words.iter().enumerate() {
        assert_eq!(names.get(i), Some(word));
    }
    assert_eq!(names.get(2), None);

    // Control 1: `.data` (the PROGBITS section at the descriptor's address) said to be 40 bytes.
    let shoff = u64_at(&so, 40) as usize;
    let shnum = u16_at(&so, 60) as usize;
    let data = (0..shnum)
        .map(|k| shoff + 64 * k)
        .find(|&s| u32_at(&so, s + 4) == 1 && u64_at(&so, s + 16) == DATA)
        .expect("W5 writes a PROGBITS section at Data's address");
    let mut cut: AVec<'static, u8> = Vec::new_in(alloc());
    cut.extend_from_slice(&so);
    cut[data + 32..data + 40].copy_from_slice(&40u64.to_le_bytes());
    assert_eq!(
        descriptor::read(&cut, Platform::LinuxArm64).map(|(d, _)| d),
        Err(DescriptorError::Truncated { what: "the descriptor (`.data` shorter than 48 bytes)", at: DATA })
    );

    // Control 2: the file ends where its section headers start.
    assert_eq!(
        descriptor::read(&so[..shoff], Platform::LinuxArm64).map(|(d, _)| d),
        Err(DescriptorError::Truncated { what: "the section headers", at: shoff as u64 })
    );

    std::println!(
        "native_so_descriptor: BNL2 read back from W5's .so equal to the image's (2 names [{}, {}]); .data cut to 40 bytes refused as Truncated; file cut before the section headers refused as Truncated",
        core::str::from_utf8(names.get(0).expect("name 0")).expect("utf-8"),
        core::str::from_utf8(names.get(1).expect("name 1")).expect("utf-8"),
    );
}
