//! A Mach-O link of a C toolchain's object whose unwind information is an `__eh_frame` FDE,
//! and the program run. `macho_unwind/eh_fde.s` says what it does: `_main` exits 0 only when
//! `_Unwind_Backtrace`, started below `_middle`, walks through `_middle` into `_main`, which
//! the unwinder can do only by reading `_middle`'s FDE (its CFA is in x19, and it leaves x29
//! alone, so neither compact unwind nor the frame-pointer chain says how).
//!
//! The FDE's `pc_begin` is the SUBTRACTOR (`ltmp3`, the start of `__eh_frame`) and UNSIGNED
//! (`_middle`) pair clang writes on arm64: the form psm's and LLVM's static libraries carry,
//! whose link stopped with "Local(..) is not defined" before the reader made it pc-relative.

use std::sync::Arc;

use blaze_codegen::Arch;
use object::{Object as _, ObjectSection as _, ObjectSegment as _, ObjectSymbol as _};

use super::{Change, Disk, Image, Native, NativeKind, TO, contents, id, name, output};
use crate::output::Platform;
use crate::{Format, Output, OutputKind};

/// `macho_unwind/eh_fde.s`, as `clang -c -arch arm64 -mmacosx-version-min=11.0` assembles it.
const OBJECT: &[u8] = include_bytes!("macho_unwind/eh_fde.o");

/// The two functions of libSystem the object calls, as the SDK's `libSystem.tbd` exports
/// them: dyld binds them by name in the real library.
const LIBSYSTEM: &str = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\n\
                         install-name: /usr/lib/libSystem.B.dylib\nexports:\n  - targets: [ arm64-macos ]\n    \
                         symbols: [ __Unwind_Backtrace, __Unwind_GetIP ]\n...\n";

fn macos() -> Output {
    Output {
        format: Format::MachO,
        arch: Arch::Aarch64,
        kind: OutputKind::Executable { entry: "main".into() },
        interpreter: None,
        platform: Some(Platform { platform: 1, minimum: 0x000b_0000, sdk: 0x000f_0000 }),
        triple: "aarch64-apple-darwin".into(),
        ..output(false)
    }
}

/// The program linked: the object, then libSystem.
fn linked() -> Vec<u8> {
    let object: Arc<[u8]> = OBJECT.into();
    let libsystem: Arc<[u8]> = LIBSYSTEM.as_bytes().into();
    let mut image = Image::new(name(TO), macos()).unwrap();
    let change = Change {
        natives: vec![
            Native {
                id: id(100),
                name: "/x/eh_fde.o",
                bytes: &object,
                kind: NativeKind::Object,
                order: 0,
            },
            Native {
                id: id(101),
                name: "/sdk/usr/lib/libSystem.tbd",
                bytes: &libsystem,
                kind: NativeKind::Shared { as_needed: false },
                order: 1,
            },
        ],
        ..Change::default()
    };
    let mut disk = Disk::default();
    disk.link(&mut image, change).unwrap_or_else(|error| panic!("linking eh_fde.o: {error}"));
    contents(disk.file(TO))
}

/// The functions each FDE of `__eh_frame` covers: where its `pc_begin` (pc-relative, 8 bytes,
/// as the object's CIE says) points.
fn fde_targets(file: &object::File<'_>) -> Vec<u64> {
    let section = file.section_by_name("__eh_frame").expect("an __eh_frame");
    let (base, bytes) = (section.address(), section.data().unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let length = u32_at(at) as usize;
        if length == 0 {
            break;
        }
        if u32_at(at + 4) != 0 {
            let field = at + 8;
            let value = i64::from_le_bytes(bytes[field..field + 8].try_into().unwrap());
            out.push((base + field as u64).wrapping_add_signed(value));
        }
        at += 4 + length;
    }
    out
}

#[test]
fn an_fde_whose_pc_begin_subtracts_a_symbol_of_eh_frame_links_and_unwinds() {
    let bytes = linked();
    let file = object::File::parse(&bytes[..]).unwrap();
    // The object's `__text` is one atom (it has no MH_SUBSECTIONS_VIA_SYMBOLS), placed whole,
    // so `_middle` is as far before the entry, `_main`, as it is in the object. The entry is
    // LC_MAIN's offset in the file, which `__TEXT` maps from its start.
    let input = object::File::parse(OBJECT).unwrap();
    let at = |name: &str| {
        input
            .symbols()
            .find(|it| it.name() == Ok(name))
            .unwrap_or_else(|| panic!("no {name}"))
            .address()
    };
    let text =
        file.segments().find(|it| it.name() == Ok(Some("__TEXT"))).expect("a __TEXT segment");
    assert_eq!(text.file_range().0, 0, "__TEXT maps the file from its start");
    let middle = text.address() + file.entry() - (at("_main") - at("_middle"));
    assert_eq!(fde_targets(&file), [middle], "the one FDE covers `_middle`");

    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("blaze-link-macho-unwind-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let program = dir.join("eh_fde");
    std::fs::write(&program, &bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let status = std::process::Command::new(&program).status().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // `_main` returns which of bit 0 (`_middle` seen) and bit 1 (`_main` seen) the walk missed.
    assert_eq!(
        status.code(),
        Some(0),
        "{}: {status}: 1 is `_middle` missed, 2 `_main`",
        program.display()
    );
}
