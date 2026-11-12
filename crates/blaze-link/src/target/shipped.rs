//! What blaze ships of a target's C library, so a link of Rust alone reads nothing the machine
//! installed (docs/v2/plan/design/toolchain-c.md, "Cross compilation"; docs/v2/plan/design/toolchain-c.md, slice 9's
//! link half):
//!
//! - glibc: which symbols each of its libraries exports at each release, from glibc's own
//!   `.abilist` files (`shipped/glibc-<arch>.abilist`, written by `scripts --bin cross`). A link
//!   for release R imports each symbol at the newest version the library has at R, as a link
//!   against that release's own `libc.so.6` would. The list is made into the stub the ELF reader
//!   takes ([`crate::elf`]'s `read::stub`), in memory: no file is written, and no `libc.so.6` of
//!   the machine is read.
//! - libgcc_s: the unwinder's interface (`shipped/libgcc_s.list`).
//! - the start of a glibc program ([`start`]): `_start`, which enters `__libc_start_main` with
//!   blaze's own runner of `.init_array`, and the `__cxa_finalize` call a shared library makes
//!   when it is unloaded. The objects `Scrt1.o`, `crti.o`, `crtn.o`, `crtbeginS.o` and
//!   `crtendS.o` have nothing else a link of Rust needs: `__dso_handle` and the array bounds are
//!   the writer's ([`crate::elf`]'s `special`), and Rust has no `.init`, `.ctors` or
//!   transactional memory.
//! - macOS: `shipped/libSystem.tbd`, the names Rust's `library/` declares that libSystem
//!   exports.
//!
//! Each is bytes of the blaze binary, so a link's inputs are the same on every machine.

use std::fmt::Write as _;
use std::sync::Arc;

use blaze_codegen::Arch;
use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind, SymbolScope,
    elf,
};

/// The first line of a glibc stub, which the ELF reader knows it by.
pub(crate) const STUB: &str = "blaze-glibc-stub 1\n";

/// libSystem's `.tbd`, for `aarch64-apple-darwin`.
pub(super) const LIBSYSTEM: &[u8] = include_bytes!("shipped/libSystem.tbd");

/// The macOS SDK `shipped/libSystem.tbd` was checked against, as `xxxx.yy.zz` nibbles: what
/// `LC_BUILD_VERSION` says the image was built with.
pub(super) const MACOS_SDK: u32 = 0x001b_0200;

const GLIBC_X86_64: &str = include_str!("shipped/glibc-x86_64.abilist");
const GLIBC_AARCH64: &str = include_str!("shipped/glibc-aarch64.abilist");
const LIBGCC_S: &str = include_str!("shipped/libgcc_s.list");

/// A glibc release, `major.minor`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Glibc(pub u16, pub u16);

impl Glibc {
    /// The release a link is for when none is asked: 2.28 (2018; Debian 10, RHEL 8), as
    /// toolchain-c.md decides.
    pub const DEFAULT: Glibc = Glibc(2, 28);

    /// `major.minor`.
    pub fn parse(text: &str) -> Option<Glibc> {
        let (major, minor) = text.split_once('.')?;
        let digits = |it: &str| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit());
        if !digits(major) || !digits(minor) {
            return None;
        }
        Some(Glibc(major.parse().ok()?, minor.parse().ok()?))
    }
}

impl std::fmt::Display for Glibc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.0, self.1)
    }
}

/// A symbol version's numbers (`2.2.5` of `GLIBC_2.2.5`), which order it.
fn version_key(version: &str) -> [u16; 3] {
    let mut key = [0; 3];
    for (at, part) in version.split('.').take(3).enumerate() {
        key[at] = part.parse().unwrap_or(0);
    }
    key
}

/// The stub of glibc's library `soname` at `release` for `arch`: each symbol the library
/// exports then, at the newest of its versions the library has then. A library that has left
/// glibc by `release` (`libpthread.so.0` from 2.34, whose symbols moved into `libc.so.6`) is a
/// stub of no symbols, which an `--as-needed` link leaves out. A release outside the list, or a
/// library not in it, is an error naming what the list has.
pub(super) fn glibc(arch: Arch, soname: &str, release: Glibc) -> Result<Arc<[u8]>, String> {
    let list = match arch {
        Arch::X86_64 => GLIBC_X86_64,
        Arch::Aarch64 => GLIBC_AARCH64,
        // alloc: error path
        Arch::Wasm32 | Arch::Wasm64 => return Err(format!("no glibc for {arch:?}")),
    };
    stub_of(list, soname, release)
}

fn stub_of(list: &str, soname: &str, release: Glibc) -> Result<Arc<[u8]>, String> {
    let mut lines = list.lines().filter(|it| !it.starts_with('#'));
    let releases = lines
        .next()
        .and_then(|it| it.strip_prefix("releases "))
        // alloc: error path
        .ok_or_else(|| "blaze's glibc list does not start with its releases".to_owned())?;
    let mut known = releases.split(' ').filter_map(Glibc::parse);
    let (first, last) = match (known.clone().next(), known.next_back()) {
        (Some(first), Some(last)) => (first, last),
        // alloc: error path
        _ => return Err("blaze's glibc list names no release".to_owned()),
    };
    if release < first || release > last {
        // alloc: error path
        return Err(format!(
            "glibc {release}: blaze ships glibc's symbols for releases {first} to {last}"
        ));
    }
    // alloc: once per process, library and release ([`super::Cache`] keeps the stub)
    let mut out = String::with_capacity(64 << 10);
    out.push_str(STUB);
    let _ = writeln!(out, "soname {soname}");
    let mut inside = false;
    let mut found = false;
    // The newest version of the name being read that the library has at `release`: lines of
    // one name are together.
    let mut held: Option<(&str, &str, &str)> = None;
    let mut flush = |held: &mut Option<(&str, &str, &str)>, out: &mut String| {
        if let Some((name, kind, version)) = held.take() {
            let _ = writeln!(out, "{kind} GLIBC_{version} {name}");
        }
    };
    for line in lines {
        if let Some(library) = line.strip_prefix("library ") {
            flush(&mut held, &mut out);
            inside = library == soname;
            found |= inside;
            continue;
        }
        if !inside {
            continue;
        }
        let mut parts = line.split(' ');
        let (Some(name), Some(kind), Some(version), Some(from)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            // alloc: error path
            return Err(format!("blaze's glibc list: a line of fewer than four fields: `{line}`"));
        };
        let until = parts.next();
        let (Some(from), until) = (Glibc::parse(from), until.map(Glibc::parse)) else {
            // alloc: error path
            return Err(format!("blaze's glibc list: `{line}` names no release"));
        };
        let Some(until) = until.unwrap_or(Some(last)) else {
            // alloc: error path
            return Err(format!("blaze's glibc list: `{line}` names no last release"));
        };
        if release < from || release > until {
            continue;
        }
        match held {
            Some((held_name, _, held_version))
                if held_name == name && version_key(held_version) >= version_key(version) => {}
            Some((held_name, ..)) if held_name != name => {
                flush(&mut held, &mut out);
                held = Some((name, kind, version));
            }
            _ => held = Some((name, kind, version)),
        }
    }
    flush(&mut held, &mut out);
    if !found {
        // alloc: error path
        return Err(format!("blaze's glibc list has no library {soname}"));
    }
    Ok(Arc::from(out.into_bytes()))
}

/// The stub of `libgcc_s.so.1`: the unwinder's interface, the same on both architectures.
pub(super) fn libgcc_s() -> Arc<[u8]> {
    // alloc: once per process ([`super::Cache`] keeps the stub)
    let mut out = String::with_capacity(1 << 10);
    out.push_str(STUB);
    out.push_str("soname libgcc_s.so.1\n");
    for line in LIBGCC_S.lines().filter(|it| !it.starts_with('#') && !it.is_empty()) {
        if let Some((name, version)) = line.split_once(' ') {
            let _ = writeln!(out, "F {version} {name}");
        }
    }
    Arc::from(out.into_bytes())
}

/// A field of [`Code`]: a relocation of `r_type` at `at` against `symbol` plus `addend`.
struct Field {
    at: u64,
    r_type: elf::RelocationType,
    symbol: &'static str,
    addend: i64,
}

const fn field(at: u64, r_type: elf::RelocationType, symbol: &'static str, addend: i64) -> Field {
    Field { at, r_type, symbol, addend }
}

/// A function of the start object: its name, its instructions and their fields.
struct Code {
    name: &'static str,
    /// Whether other objects see it (`_start`) or only this one.
    global: bool,
    code: &'static [u8],
    fields: &'static [Field],
}

const fn words_le<const N: usize, const B: usize>(words: [u32; N]) -> [u8; B] {
    assert!(B == N * 4);
    let mut out = [0; B];
    let mut at = 0;
    while at < N {
        let bytes = words[at].to_le_bytes();
        out[at * 4] = bytes[0];
        out[at * 4 + 1] = bytes[1];
        out[at * 4 + 2] = bytes[2];
        out[at * 4 + 3] = bytes[3];
        at += 1;
    }
    out
}

/// What the start of a glibc program runs its initializers with: `__libc_start_main`'s `init`,
/// called with `(argc, argv, envp)` once libc is set up, as `__libc_csu_init` of glibc's
/// `libc_nonshared.a` was until 2.34. Every glibc calls an `init` it is given (2.34 and later
/// as "a legacy program which supplied its own init routine") and runs the executable's
/// `.init_array` itself only when given none, so the array runs once on every release. The
/// loader runs `.preinit_array`, and a link of Rust has no `_init`.
const INIT: &str = "__blaze_init";

/// The `.fini_array` entry of every image: `__cxa_finalize(&__dso_handle)`, which runs what the
/// image registered with `__cxa_atexit` and its thread-local destructors' bookkeeping when a
/// shared library is unloaded, as `crtbeginS.o`'s `__do_global_dtors_aux` does. In an
/// executable it runs at exit, after `exit` has run every handler, and finds nothing left.
const FINI: &str = "__blaze_finalize";

/// x86-64 System V. The kernel enters `_start` with `argc` at `[rsp]`, `argv` after it and the
/// loader's exit function in `rdx` (glibc's sysdeps/x86_64/start.S is the reference).
const X86_START: [u8; 38] = [
    0x31, 0xed, // xor ebp, ebp
    0x49, 0x89, 0xd1, // mov r9, rdx            (rtld_fini)
    0x5e, // pop rsi                            (argc)
    0x48, 0x89, 0xe2, // mov rdx, rsp           (argv)
    0x48, 0x83, 0xe4, 0xf0, // and rsp, -16
    0x50, // push rax
    0x54, // push rsp                           (stack_end)
    0x45, 0x31, 0xc0, // xor r8d, r8d           (fini: none)
    0x48, 0x8d, 0x0d, 0, 0, 0, 0, // lea rcx, [rip + __blaze_init]
    0x48, 0x8d, 0x3d, 0, 0, 0, 0, // lea rdi, [rip + main]
    0xe8, 0, 0, 0, 0, // call __libc_start_main
    0xf4, // hlt
];
const X86_START_FIELDS: [Field; 3] = [
    field(21, elf::R_X86_64_PC32, INIT, -4),
    field(28, elf::R_X86_64_PC32, "main", -4),
    field(33, elf::R_X86_64_PLT32, "__libc_start_main", -4),
];

/// x86-64: `init(edi argc, rsi argv, rdx envp)`. Five pushes leave the stack 16-aligned for
/// each call.
const X86_INIT: [u8; 62] = [
    0x41, 0x54, // push r12
    0x41, 0x55, // push r13
    0x41, 0x56, // push r14
    0x53, // push rbx
    0x55, // push rbp
    0x41, 0x89, 0xfc, // mov r12d, edi
    0x49, 0x89, 0xf5, // mov r13, rsi
    0x49, 0x89, 0xd6, // mov r14, rdx
    0x48, 0x8d, 0x1d, 0, 0, 0, 0, // lea rbx, [rip + __init_array_start]
    0x48, 0x8d, 0x2d, 0, 0, 0, 0, // lea rbp, [rip + __init_array_end]
    // loop:
    0x48, 0x39, 0xeb, // cmp rbx, rbp
    0x73, 0x11, // jae done
    0x44, 0x89, 0xe7, // mov edi, r12d
    0x4c, 0x89, 0xee, // mov rsi, r13
    0x4c, 0x89, 0xf2, // mov rdx, r14
    0xff, 0x13, // call [rbx]
    0x48, 0x83, 0xc3, 0x08, // add rbx, 8
    0xeb, 0xea, // jmp loop
    // done:
    0x5d, // pop rbp
    0x5b, // pop rbx
    0x41, 0x5e, // pop r14
    0x41, 0x5d, // pop r13
    0x41, 0x5c, // pop r12
    0xc3, // ret
];
const X86_INIT_FIELDS: [Field; 2] = [
    field(20, elf::R_X86_64_PC32, "__init_array_start", -4),
    field(27, elf::R_X86_64_PC32, "__init_array_end", -4),
];

/// x86-64: `__cxa_finalize(&__dso_handle)`, as a tail call.
const X86_FINI: [u8; 12] = [
    0x48, 0x8d, 0x3d, 0, 0, 0, 0, // lea rdi, [rip + __dso_handle]
    0xe9, 0, 0, 0, 0, // jmp __cxa_finalize
];
const X86_FINI_FIELDS: [Field; 2] = [
    field(3, elf::R_X86_64_PC32, "__dso_handle", -4),
    field(8, elf::R_X86_64_PLT32, "__cxa_finalize", -4),
];

/// AArch64. The kernel enters `_start` with `argc` at `[sp]`, `argv` after it and the loader's
/// exit function in `x0` (glibc's sysdeps/aarch64/start.S is the reference).
const A64_START: [u8; 52] = words_le([
    0xd280_001d, // mov x29, #0
    0xd280_001e, // mov x30, #0
    0xaa00_03e5, // mov x5, x0                 (rtld_fini)
    0xf940_03e1, // ldr x1, [sp]               (argc)
    0x9100_23e2, // add x2, sp, #8             (argv)
    0x9100_03e6, // mov x6, sp                 (stack_end)
    0x9000_0000, // adrp x0, main
    0x9100_0000, // add x0, x0, :lo12:main
    0x9000_0003, // adrp x3, __blaze_init
    0x9100_0063, // add x3, x3, :lo12:__blaze_init
    0xd280_0004, // mov x4, #0                 (fini: none)
    0x9400_0000, // bl __libc_start_main
    0xd420_0020, // brk #1
]);
const A64_START_FIELDS: [Field; 5] = [
    field(24, elf::R_AARCH64_ADR_PREL_PG_HI21, "main", 0),
    field(28, elf::R_AARCH64_ADD_ABS_LO12_NC, "main", 0),
    field(32, elf::R_AARCH64_ADR_PREL_PG_HI21, INIT, 0),
    field(36, elf::R_AARCH64_ADD_ABS_LO12_NC, INIT, 0),
    field(44, elf::R_AARCH64_CALL26, "__libc_start_main", 0),
];

/// AArch64: `init(w0 argc, x1 argv, x2 envp)`.
const A64_INIT: [u8; 100] = words_le([
    0xa9bc_7bfd, // stp x29, x30, [sp, #-64]!
    0x9100_03fd, // mov x29, sp
    0xa901_53f3, // stp x19, x20, [sp, #16]
    0xa902_5bf5, // stp x21, x22, [sp, #32]
    0xf900_1bf7, // str x23, [sp, #48]
    0x2a00_03f5, // mov w21, w0
    0xaa01_03f6, // mov x22, x1
    0xaa02_03f7, // mov x23, x2
    0x9000_0013, // adrp x19, __init_array_start
    0x9100_0273, // add x19, x19, :lo12:__init_array_start
    0x9000_0014, // adrp x20, __init_array_end
    0x9100_0294, // add x20, x20, :lo12:__init_array_end
    // loop:
    0xeb14_027f, // cmp x19, x20
    0x5400_00e2, // b.hs done
    0xf840_8668, // ldr x8, [x19], #8
    0x2a15_03e0, // mov w0, w21
    0xaa16_03e1, // mov x1, x22
    0xaa17_03e2, // mov x2, x23
    0xd63f_0100, // blr x8
    0x17ff_fff9, // b loop
    // done:
    0xf940_1bf7, // ldr x23, [sp, #48]
    0xa942_5bf5, // ldp x21, x22, [sp, #32]
    0xa941_53f3, // ldp x19, x20, [sp, #16]
    0xa8c4_7bfd, // ldp x29, x30, [sp], #64
    0xd65f_03c0, // ret
]);
const A64_INIT_FIELDS: [Field; 4] = [
    field(32, elf::R_AARCH64_ADR_PREL_PG_HI21, "__init_array_start", 0),
    field(36, elf::R_AARCH64_ADD_ABS_LO12_NC, "__init_array_start", 0),
    field(40, elf::R_AARCH64_ADR_PREL_PG_HI21, "__init_array_end", 0),
    field(44, elf::R_AARCH64_ADD_ABS_LO12_NC, "__init_array_end", 0),
];

/// AArch64: `__cxa_finalize(&__dso_handle)`, as a tail call.
const A64_FINI: [u8; 12] = words_le([
    0x9000_0000, // adrp x0, __dso_handle
    0x9100_0000, // add x0, x0, :lo12:__dso_handle
    0x1400_0000, // b __cxa_finalize
]);
const A64_FINI_FIELDS: [Field; 3] = [
    field(0, elf::R_AARCH64_ADR_PREL_PG_HI21, "__dso_handle", 0),
    field(4, elf::R_AARCH64_ADD_ABS_LO12_NC, "__dso_handle", 0),
    field(8, elf::R_AARCH64_JUMP26, "__cxa_finalize", 0),
];

/// What `libc_nonshared.a` gave a program linked for a glibc before 2.33, where `stat` and its
/// kin were not symbols of `libc.so.6` but wrappers linked into each program around `__xstat`
/// and its kin, which take the layout's version first: `(name, the wrapped function, whether it
/// is `fstatat`'s four arguments)`. Rust's std calls `fstat64`, `stat64` and `lstat64` by name.
const STATS: [(&str, &str, bool); 8] = [
    ("stat", "__xstat", false),
    ("stat64", "__xstat64", false),
    ("lstat", "__lxstat", false),
    ("lstat64", "__lxstat64", false),
    ("fstat", "__fxstat", false),
    ("fstat64", "__fxstat64", false),
    ("fstatat", "__fxstatat", true),
    ("fstatat64", "__fxstatat64", true),
];

/// The first release whose `libc.so.6` has [`STATS`]' names itself.
const STATS_IN_LIBC: Glibc = Glibc(2, 33);

/// x86-64, `_STAT_VER` 1: `f(a, b)` as `g(1, a, b)`, a tail call. The field is at 12.
const X86_STAT: [u8; 16] = [
    0x48, 0x89, 0xf2, // mov rdx, rsi
    0x48, 0x89, 0xfe, // mov rsi, rdi
    0xbf, 0x01, 0x00, 0x00, 0x00, // mov edi, 1
    0xe9, 0, 0, 0, 0, // jmp g
];
/// x86-64: `f(a, b, c, d)` as `g(1, a, b, c, d)`. The field is at 18.
const X86_STATAT: [u8; 22] = [
    0x49, 0x89, 0xc8, // mov r8, rcx
    0x48, 0x89, 0xd1, // mov rcx, rdx
    0x48, 0x89, 0xf2, // mov rdx, rsi
    0x48, 0x89, 0xfe, // mov rsi, rdi
    0xbf, 0x01, 0x00, 0x00, 0x00, // mov edi, 1
    0xe9, 0, 0, 0, 0, // jmp g
];
/// AArch64, `_STAT_VER` 0: `f(a, b)` as `g(0, a, b)`. The field is at 12.
const A64_STAT: [u8; 16] = words_le([
    0xaa01_03e2, // mov x2, x1
    0xaa00_03e1, // mov x1, x0
    0x5280_0000, // mov w0, #0
    0x1400_0000, // b g
]);
/// AArch64: `f(a, b, c, d)` as `g(0, a, b, c, d)`. The field is at 20.
const A64_STATAT: [u8; 24] = words_le([
    0xaa03_03e4, // mov x4, x3
    0xaa02_03e3, // mov x3, x2
    0xaa01_03e2, // mov x2, x1
    0xaa00_03e1, // mov x1, x0
    0x5280_0000, // mov w0, #0
    0x1400_0000, // b g
]);

/// The start object of a glibc image for `arch`, as a relocatable ELF object: [`FINI`] and its
/// `.fini_array` entry, for an `executable` `_start` and [`INIT`], and for a `release` before
/// 2.33 [`STATS`]. Its bytes depend on its arguments alone.
pub(super) fn start(arch: Arch, executable: bool, release: Glibc) -> Result<Arc<[u8]>, String> {
    let (architecture, absolute, start, init, fini) = match arch {
        Arch::X86_64 => (
            Architecture::X86_64,
            elf::R_X86_64_64,
            Code { name: "_start", global: true, code: &X86_START, fields: &X86_START_FIELDS },
            Code { name: INIT, global: false, code: &X86_INIT, fields: &X86_INIT_FIELDS },
            Code { name: FINI, global: false, code: &X86_FINI, fields: &X86_FINI_FIELDS },
        ),
        Arch::Aarch64 => (
            Architecture::Aarch64,
            elf::R_AARCH64_ABS64,
            Code { name: "_start", global: true, code: &A64_START, fields: &A64_START_FIELDS },
            Code { name: INIT, global: false, code: &A64_INIT, fields: &A64_INIT_FIELDS },
            Code { name: FINI, global: false, code: &A64_FINI, fields: &A64_FINI_FIELDS },
        ),
        // alloc: error path
        Arch::Wasm32 | Arch::Wasm64 => return Err(format!("no glibc start for {arch:?}")),
    };
    let mut object = Object::new(BinaryFormat::Elf, architecture, Endianness::Little);
    let text = object.section_id(StandardSection::Text);
    // alloc: once per process and kind of image, the start object's few names
    let mut undefined: Vec<(&'static str, object::write::SymbolId)> = Vec::new();
    let mut defined: Vec<(&'static str, object::write::SymbolId)> = Vec::new();
    let codes: &[&Code] = match executable {
        true => &[&fini, &init, &start],
        false => &[&fini],
    };
    for code in codes {
        let at = object.append_section_data(text, code.code, 16);
        let symbol = object.add_symbol(Symbol {
            name: code.name.as_bytes().to_vec(),
            value: at,
            size: code.code.len() as u64,
            kind: SymbolKind::Text,
            scope: if code.global { SymbolScope::Dynamic } else { SymbolScope::Compilation },
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
        defined.push((code.name, symbol));
    }
    if release < STATS_IN_LIBC {
        let (plain, at_form, jump, plain_at, at_form_at): (&[u8], &[u8], elf::RelocationType, u64, u64) =
            match arch
        {
            Arch::X86_64 => (&X86_STAT, &X86_STATAT, elf::R_X86_64_PLT32, 12, 18),
            _ => (&A64_STAT, &A64_STATAT, elf::R_AARCH64_JUMP26, 12, 20),
        };
        let addend = if arch == Arch::X86_64 { -4 } else { 0 };
        let stats = object.add_section(Vec::new(), b".text.stat".to_vec(), object::SectionKind::Text);
        for (name, wrapped, four) in STATS {
            let (code, field) = if four { (at_form, at_form_at) } else { (plain, plain_at) };
            let at = object.append_section_data(stats, code, 16);
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: at,
                size: code.len() as u64,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(stats),
                flags: SymbolFlags::None,
            });
            let wrapped = object.add_symbol(Symbol {
                name: wrapped.as_bytes().to_vec(),
                value: 0,
                size: 0,
                kind: SymbolKind::Unknown,
                scope: SymbolScope::Unknown,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            });
            object
                .add_relocation(
                    stats,
                    Relocation {
                        offset: at + field,
                        symbol: wrapped,
                        addend,
                        flags: RelocationFlags::Elf { r_type: jump },
                    },
                )
                // alloc: error path
                .map_err(|error| format!("the start object: {error}"))?;
        }
    }
    let mut at = 0;
    for code in codes {
        let base = at;
        at += (code.code.len() as u64).next_multiple_of(16);
        for field in code.fields {
            let symbol = match defined.iter().chain(&undefined).find(|it| it.0 == field.symbol) {
                Some(it) => it.1,
                None => {
                    let symbol = object.add_symbol(Symbol {
                        name: field.symbol.as_bytes().to_vec(),
                        value: 0,
                        size: 0,
                        kind: SymbolKind::Unknown,
                        scope: SymbolScope::Unknown,
                        weak: false,
                        section: SymbolSection::Undefined,
                        flags: SymbolFlags::None,
                    });
                    undefined.push((field.symbol, symbol));
                    symbol
                }
            };
            object
                .add_relocation(
                    text,
                    Relocation {
                        offset: base + field.at,
                        symbol,
                        addend: field.addend,
                        flags: RelocationFlags::Elf { r_type: field.r_type },
                    },
                )
                // alloc: error path
                .map_err(|error| format!("the start object: {error}"))?;
        }
    }
    let array = object.add_section(Vec::new(), b".fini_array".to_vec(), object::SectionKind::Data);
    object.section_mut(array).flags = object::SectionFlags::Elf {
        sh_type: elf::SHT_FINI_ARRAY,
        sh_flags: elf::SHF_ALLOC | elf::SHF_WRITE,
    };
    let entry = object.append_section_data(array, &[0; 8], 8);
    let finalize = defined[0].1;
    object
        .add_relocation(
            array,
            Relocation {
                offset: entry,
                symbol: finalize,
                addend: 0,
                flags: RelocationFlags::Elf { r_type: absolute },
            },
        )
        // alloc: error path
        .map_err(|error| format!("the start object: {error}"))?;
    // alloc: error path
    let bytes = object.write().map_err(|error| format!("the start object: {error}"))?;
    Ok(Arc::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "# a comment\nreleases 2.17 2.18 2.19 2.20\nlibrary libc.so.6\n\
        environ D 2.2.5 2.17\nmemcpy F 2.2.5 2.17\nmemcpy F 2.14 2.17\nnewer F 2.19 2.19\n\
        moved F 2.2.5 2.19\nmoved F 2.19 2.19\nlibrary libdl.so.2\nmoved F 2.2.5 2.17 2.18\n";

    fn text(list: &str, soname: &str, release: Glibc) -> String {
        String::from_utf8(stub_of(list, soname, release).unwrap().to_vec()).unwrap()
    }

    #[test]
    fn a_stub_has_each_symbol_at_the_newest_version_its_library_has_at_the_release() {
        assert_eq!(
            text(LIST, "libc.so.6", Glibc(2, 18)),
            "blaze-glibc-stub 1\nsoname libc.so.6\nD GLIBC_2.2.5 environ\nF GLIBC_2.14 memcpy\n"
        );
        assert_eq!(
            text(LIST, "libc.so.6", Glibc(2, 20)),
            "blaze-glibc-stub 1\nsoname libc.so.6\nD GLIBC_2.2.5 environ\nF GLIBC_2.14 memcpy\n\
             F GLIBC_2.19 newer\nF GLIBC_2.19 moved\n"
        );
        assert_eq!(
            text(LIST, "libdl.so.2", Glibc(2, 18)),
            "blaze-glibc-stub 1\nsoname libdl.so.2\nF GLIBC_2.2.5 moved\n"
        );
        assert_eq!(text(LIST, "libdl.so.2", Glibc(2, 19)), "blaze-glibc-stub 1\nsoname libdl.so.2\n");
    }

    #[test]
    fn a_release_or_library_the_list_lacks_is_an_error_naming_what_it_has() {
        let error = stub_of(LIST, "libc.so.6", Glibc(2, 16)).unwrap_err();
        assert!(error.contains("2.16") && error.contains("2.17 to 2.20"), "{error}");
        let error = stub_of(LIST, "libc.so.6", Glibc(2, 21)).unwrap_err();
        assert!(error.contains("2.21"), "{error}");
        let error = stub_of(LIST, "libnope.so.1", Glibc(2, 18)).unwrap_err();
        assert!(error.contains("libnope.so.1"), "{error}");
    }

    /// The shipped lists hold what Rust's std links at the default release, on both
    /// architectures: the libraries the `libc` and `unwind` crates name, and the symbols the
    /// start object calls.
    #[test]
    fn the_shipped_glibc_lists_have_what_the_start_object_and_std_name() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let libc = glibc(arch, "libc.so.6", Glibc::DEFAULT).unwrap();
            let libc = std::str::from_utf8(&libc).unwrap();
            let wrapped = STATS.iter().map(|it| it.1);
            for name in ["__libc_start_main", "__cxa_finalize", "malloc", "environ", "write"]
                .into_iter()
                .chain(wrapped)
            {
                assert!(libc.lines().any(|it| it.rsplit(' ').next() == Some(name)), "{name}");
            }
            // Before 2.34 threads are libpthread's and `dlsym` is libdl's.
            for (soname, name) in [("libpthread.so.0", "pthread_create"), ("libdl.so.2", "dlsym")] {
                let old = glibc(arch, soname, Glibc::DEFAULT).unwrap();
                let old = std::str::from_utf8(&old).unwrap();
                assert!(old.lines().any(|it| it.rsplit(' ').next() == Some(name)), "{name}");
                let new = glibc(arch, soname, Glibc(2, 34)).unwrap();
                assert_eq!(new.len(), STUB.len() + "soname \n".len() + soname.len(), "{soname}");
            }
            for soname in ["libm.so.6", "librt.so.1", "libutil.so.1", "libresolv.so.2"] {
                glibc(arch, soname, Glibc::DEFAULT).unwrap();
            }
        }
        assert!(std::str::from_utf8(&libgcc_s()).unwrap().contains("F GCC_3.0 _Unwind_Resume\n"));
    }

    fn has_name(bytes: &[u8], name: &[u8]) -> bool {
        bytes.windows(name.len()).any(|it| it == name)
    }

    /// The start objects are deterministic, and each relocation is inside its function.
    #[test]
    fn the_start_object_is_the_same_bytes_each_time_and_defines_start_only_for_an_executable() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let executable = start(arch, true, Glibc::DEFAULT).unwrap();
            assert_eq!(executable, start(arch, true, Glibc::DEFAULT).unwrap());
            let shared = start(arch, false, Glibc::DEFAULT).unwrap();
            let later = start(arch, true, STATS_IN_LIBC).unwrap();
            assert!(has_name(&executable, b"fstat64\0") && !has_name(&later, b"fstat64\0"));
            let has = has_name;
            assert!(has(&executable, b"_start\0") && has(&executable, b"__libc_start_main\0"));
            assert!(!has(&shared, b"_start\0") && has(&shared, b"__cxa_finalize\0"));
        }
        for (code, fields) in [
            (&X86_START[..], &X86_START_FIELDS[..]),
            (&X86_INIT[..], &X86_INIT_FIELDS[..]),
            (&X86_FINI[..], &X86_FINI_FIELDS[..]),
            (&A64_START[..], &A64_START_FIELDS[..]),
            (&A64_INIT[..], &A64_INIT_FIELDS[..]),
            (&A64_FINI[..], &A64_FINI_FIELDS[..]),
        ] {
            for field in fields {
                assert!(field.at as usize + 4 <= code.len(), "{}", field.symbol);
            }
        }
    }
}
