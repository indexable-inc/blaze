//! AArch64 TLS descriptors of a C toolchain's object: relaxed in an executable, kept in a
//! shared library, and the executable run.
//!
//! `tls_desc/main.c` reaches three thread-locals through descriptors (what `-fPIC` code does
//! on AArch64, and what libstdc++'s `std::call_once` does to `__once_callable`): two of its
//! own and one `libtv.so` defines. Linking it stopped at the `R_AARCH64_TLSDESC_CALL` of the
//! imported one ("is imported: reaching it this way needs a copy relocation"), the one
//! relocation of the four that was not known to reach its target through the GOT.

use std::sync::Arc;

use blaze_codegen::Arch;
use object::read::elf::{ElfFile64, FileHeader as _, ProgramHeader as _};
use object::{Endianness, Object as _, ObjectSection as _, ObjectSymbol as _, elf};

use super::{Change, Disk, Image, Native, NativeKind, contents, id, name, output};
use crate::{LinkError, Output, OutputKind};

/// `tls_desc/main.c` and `tls_desc/tv.c`, as gcc 16.2 compiles them (the command is in each).
const MAIN: &[u8] = include_bytes!("tls_desc/main.o");
const TV: &[u8] = include_bytes!("tls_desc/tv.o");

const ADRP_X0: u32 = 0x9000_0000;
const NOP: u32 = 0xd503_201f;
/// `blr x1`: the call of a descriptor's resolver.
const BLR_X1: u32 = 0xd63f_0020;

fn linux(kind: OutputKind, interpreter: Option<String>) -> Output {
    Output {
        arch: Arch::Aarch64,
        kind,
        interpreter,
        triple: "aarch64-unknown-linux-gnu".into(),
        ..output(false)
    }
}

fn link(to: &str, output: Output, natives: Vec<Native<'_>>) -> Result<Vec<u8>, LinkError> {
    let mut image = Image::new(name(to), output).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, Change { natives, ..Change::default() })?;
    Ok(contents(disk.file(to)))
}

/// `libtv.so`, linked by the linker itself: it exports the thread-local `imported`.
fn library() -> Arc<[u8]> {
    let kind = OutputKind::Shared { name: "libtv.so".into(), exports: vec!["imported".into()] };
    let tv: Arc<[u8]> = TV.into();
    let object =
        Native { id: id(101), name: "/x/tv.o", bytes: &tv, kind: NativeKind::Object, order: 0 };
    link("out/libtv.so", linux(kind, None), vec![object])
        .unwrap_or_else(|error| panic!("linking libtv.so: {error}"))
        .into()
}

/// `main.o` linked against `libtv.so` as an image of `kind`.
fn linked(kind: OutputKind, interpreter: Option<String>, library: &Arc<[u8]>) -> Vec<u8> {
    let main: Arc<[u8]> = MAIN.into();
    let natives = vec![
        Native { id: id(100), name: "/x/main.o", bytes: &main, kind: NativeKind::Object, order: 0 },
        Native {
            id: id(102),
            name: "/x/libtv.so",
            bytes: library,
            kind: NativeKind::Shared { as_needed: false },
            order: 1,
        },
    ];
    link("out/app", linux(kind, interpreter), natives)
        .unwrap_or_else(|error| panic!("linking main.o: {error}"))
}

/// The image's code, as instruction words.
fn code(file: &ElfFile64<'_, Endianness>) -> Vec<u32> {
    file.sections()
        .filter(|it| it.kind() == object::SectionKind::Text)
        .flat_map(|it| it.data().unwrap().chunks_exact(4).map(to_u32).collect::<Vec<_>>())
        .collect()
}

fn to_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

/// The offsets from the thread pointer of the sequences relaxed to local exec: `movz x0,
/// #hi16, lsl #16; movk x0, #lo16; nop; nop`.
fn local_exec(words: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = words
        .windows(4)
        .filter(|it| {
            it[0] & 0xffe0_001f == 0xd2a0_0000
                && it[1] & 0xffe0_001f == 0xf280_0000
                && it[2..] == [NOP, NOP]
        })
        .map(|it| (((it[0] >> 5) & 0xffff) << 16) | ((it[1] >> 5) & 0xffff))
        .collect();
    out.sort_unstable();
    out
}

/// How many sequences are relaxed to initial exec: `adrp x0, cell; ldr x0, [x0, cell]; nop;
/// nop`.
fn initial_exec(words: &[u32]) -> usize {
    words
        .windows(4)
        .filter(|it| {
            it[0] & 0x9f00_001f == ADRP_X0
                && it[1] & 0xffc0_03ff == 0xf940_0000
                && it[2..] == [NOP, NOP]
        })
        .count()
}

/// The dynamic relocations of `r_type`, each as the name of its symbol ("" for none).
fn dynamic(file: &ElfFile64<'_, Endianness>, r_type: u32) -> Vec<String> {
    use object::ObjectSymbolTable as _;
    let mut out: Vec<String> = file
        .dynamic_relocations()
        .expect("dynamic relocations")
        .filter(|(_, it)| it.flags() == object::RelocationFlags::Elf { r_type })
        .map(|(_, it)| match it.target() {
            object::RelocationTarget::Symbol(index) => file
                .dynamic_symbol_table()
                .and_then(|table| table.symbol_by_index(index).ok())
                .and_then(|symbol| symbol.name().ok())
                .unwrap_or("")
                .to_owned(),
            _ => String::new(),
        })
        .collect();
    out.sort();
    out
}

/// The program interpreter of the test's own executable, which is this host's loader.
fn interpreter() -> String {
    let own = std::fs::read("/proc/self/exe").expect("the test's own executable");
    let header = elf::FileHeader64::<Endianness>::parse(&own[..]).unwrap();
    let endian = header.endian().unwrap();
    let interp = header
        .program_headers(endian, &own[..])
        .unwrap()
        .iter()
        .find_map(|it| it.interpreter(endian, &own[..]).ok().flatten())
        .expect("the test's own executable names a program interpreter (PT_INTERP)");
    String::from_utf8(interp.to_vec()).unwrap()
}

/// The rule (the AArch64 ELF TLS ABI's descriptor relaxations): in an executable, a
/// descriptor's four instructions become `movz x0; movk x0; nop; nop` of the offset from the
/// thread pointer for a thread-local of the image (local exec), and `adrp x0; ldr x0, [x0];
/// nop; nop` through a GOT cell the loader fills (`R_AARCH64_TLS_TPREL64`) for an imported
/// one (initial exec). No descriptor is left, and the program reads and writes each
/// thread-local at the place the loader put it.
#[test]
fn an_executables_tls_descriptors_are_relaxed_and_it_reads_its_thread_locals() {
    let library = library();
    let host = cfg!(all(target_os = "linux", target_arch = "aarch64"));
    let interp = if host { interpreter() } else { "/lib/ld-linux-aarch64.so.1".to_owned() };
    let kind = OutputKind::Executable { entry: "_start".into() };
    let bytes = linked(kind, Some(interp), &library);
    let file = ElfFile64::<Endianness>::parse(&bytes[..]).unwrap();
    let words = code(&file);

    // Local exec: `own` is first in the TLS block, 16 bytes (the control block) from the
    // thread pointer, and `zeroed` is after it at its alignment of 8.
    assert_eq!(
        local_exec(&words),
        [16, 16, 24, 24],
        "`own` and `zeroed`, each read in two places"
    );
    // Initial exec: `imported`, read in two places through one cell.
    assert_eq!(initial_exec(&words), 2, "`imported`, read in two places");
    assert!(!words.contains(&BLR_X1), "no descriptor is called");
    assert_eq!(dynamic(&file, elf::R_AARCH64_TLS_TPREL64.0), ["imported"]);
    assert_eq!(dynamic(&file, elf::R_AARCH64_TLSDESC.0), [""; 0]);

    if !host {
        return;
    }
    let dir = std::env::temp_dir().join(format!("blaze-link-tls-desc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let program = dir.join("app");
    std::fs::write(&program, &bytes).unwrap();
    std::fs::write(dir.join("libtv.so"), &library[..]).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let status =
        std::process::Command::new(&program).env("LD_LIBRARY_PATH", &dir).status().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // `_start` exits with a bit for each read that was wrong (tls_desc/main.c).
    assert_eq!(
        status.code(),
        Some(0),
        "{}: {status}: bits 1 and 2 are `own`, 4 and 8 `zeroed`, 16 and 32 `imported`, 64 two \
         thread-locals at one address",
        program.display()
    );
}

/// The control: the same object in a shared library, where nothing is relaxed, keeps every
/// descriptor, so the relaxation is what removed them above. Its thread-locals' places are
/// the loader's to say: one `R_AARCH64_TLSDESC` pair for each of the three, the imported
/// one's by name, and all six calls left.
#[test]
fn a_shared_librarys_tls_descriptors_are_kept() {
    let library = library();
    let exports = ["own_at", "zeroed_at", "imported_at"].map(str::to_owned).to_vec();
    let bytes = linked(OutputKind::Shared { name: "libapp.so".into(), exports }, None, &library);
    let file = ElfFile64::<Endianness>::parse(&bytes[..]).unwrap();
    let words = code(&file);
    assert_eq!(words.iter().filter(|it| **it == BLR_X1).count(), 6, "each descriptor is called");
    assert_eq!((local_exec(&words), initial_exec(&words)), (Vec::new(), 0), "none is relaxed");
    assert_eq!(dynamic(&file, elf::R_AARCH64_TLSDESC.0), ["", "", "imported"]);
    assert_eq!(dynamic(&file, elf::R_AARCH64_TLS_TPREL64.0), [""; 0]);
}
