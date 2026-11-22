//! The arguments the pinned clang's driver is run with, inside the helper and over its virtual
//! file system: every path a guest path (a mount's spelling and a path under it,
//! `blaze_roots::Mounts`), nothing of the machine, the same bytes for every builder of one
//! unit (rule 25). Why each is there is c.md "The toolchain side", "Hermetic by construction".

#![blaze::sans_io]

use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_roots::RelPath;

use crate::wire::Mode;

/// The pinned clang's major version: its resource directory is `lib/clang/<major>`
/// (pins/clang.json `version` 23.1.4).
pub const CLANG_MAJOR: &str = "23";

/// The guest path of the synthesized main file of an import (helper/main.cpp answers it
/// itself: it is a function of the arguments, not an input).
pub const IMPORT_MAIN: &str = "/.blaze/import.c";

/// The source language.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    C,
    Cxx,
}

/// The code made: dependencies are made fast, workspace units quickly (AGENTS.md "Scope").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opt {
    /// `-O0`.
    Quick,
    /// `-O2`.
    Fast,
}

/// The debug information kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Debug {
    None,
    /// Line tables alone.
    Lines,
}

/// The target a unit is compiled for: the hosts in scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// `aarch64-apple-darwin`, for macOS `min_major.min_minor` and later.
    MacosArm64 { min_major: u16, min_minor: u16 },
    LinuxArm64,
    LinuxX64,
}

impl Target {
    pub fn triple(self) -> &'static str {
        match self {
            Target::MacosArm64 { .. } => "aarch64-apple-darwin",
            Target::LinuxArm64 => "aarch64-unknown-linux-gnu",
            Target::LinuxX64 => "x86_64-unknown-linux-gnu",
        }
    }
}

/// A preprocessor definition: `NAME` or `NAME=value`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Define<'u> {
    pub name: &'u str,
    pub value: Option<&'u str>,
}

/// What a unit reads first.
#[derive(Clone, Copy, Debug)]
pub enum Source<'u> {
    /// A translation unit's file, under the package.
    File(RelPath<'u>),
    /// An import's headers, under the package, each `-include`d into the empty
    /// [`IMPORT_MAIN`] in order: the declarations of all of them, one parse.
    Headers(&'u [RelPath<'u>]),
}

/// Where the toolchain and the package are seen, as guest spellings (each a mount's spelling,
/// or a directory beneath one: `Mounts::spelling_of` and a path).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Toolchain<'s> {
    /// The pinned clang's tree: `bin/clang`, `lib/clang/<major>/include`, `include/c++/v1`.
    pub clang: &'s str,
    /// The target's headers: the macOS SDK (read by version, its digest in the key: c.md
    /// decision 1) or the pinned Linux sysroot archive.
    pub sysroot: &'s str,
    /// The package's root.
    pub package: &'s str,
}

/// One translation unit or import, as its key names it.
#[derive(Clone, Copy, Debug)]
pub struct Unit<'u> {
    pub lang: Lang,
    /// `c11`, `gnu17`, `c++17`, ...: one of [`STANDARDS`].
    pub std: &'u str,
    pub target: Target,
    pub source: Source<'u>,
    /// Its include directories, under the package, in search order.
    pub include: &'u [RelPath<'u>],
    pub defines: &'u [Define<'u>],
    pub opt: Opt,
    pub debug: Debug,
}

/// The language standards a unit may name.
pub const STANDARDS: &[&str] = &[
    "c89", "c99", "c11", "c17", "c23", "gnu89", "gnu99", "gnu11", "gnu17", "gnu23", "c++11", "c++14", "c++17",
    "c++20", "c++23", "c++26", "gnu++11", "gnu++14", "gnu++17", "gnu++20", "gnu++23", "gnu++26",
];

/// Why a unit's arguments could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlagFault {
    /// A standard not in [`STANDARDS`], or one of the other language.
    Standard,
    /// A toolchain spelling that is not absolute and canonical (`/a/b`, no trailing `/`).
    Spelling,
    /// A definition whose name is not an identifier, or one of the names the flags fix
    /// (`__DATE__`, `__TIME__`, `__TIMESTAMP__`).
    DefineName,
    /// A definition's value with a NUL or a line break.
    DefineValue,
    /// An import with no header, or headers given for a compile.
    Source,
}

/// The builtin macros that would read a clock, each fixed to a constant (no `SOURCE_DATE_EPOCH`
/// is read: the environment is empty).
const FIXED: &[&str] = &["__DATE__=\"Jan  1 1970\"", "__TIME__=\"00:00:00\"", "__TIMESTAMP__=\"Thu Jan  1 00:00:00 1970\""];

/// The driver's arguments, the program first, for `unit` in `mode`.
pub fn args<'a>(unit: &Unit<'_>, tools: Toolchain<'_>, mode: Mode, alloc: Alloc<'a>) -> Result<AVec<'a, AString<'a>>, FlagFault> {
    for spelling in [tools.clang, tools.sysroot, tools.package] {
        if !canonical_absolute(spelling) {
            return Err(FlagFault::Spelling);
        }
    }
    let cxx = unit.lang == Lang::Cxx;
    if !STANDARDS.contains(&unit.std) || unit.std.contains("++") != cxx {
        return Err(FlagFault::Standard);
    }
    match (unit.source, mode) {
        (Source::Headers(headers), Mode::Decls) if !headers.is_empty() => {}
        (Source::File(_), _) => {}
        _ => return Err(FlagFault::Source),
    }
    let mut a: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let mut push = |parts: &[&str]| {
        let mut s = String::new_in(alloc);
        for part in parts {
            s.push_str(part);
        }
        a.push(s);
    };
    push(&[tools.clang, if cxx { "/bin/clang++" } else { "/bin/clang" }]);
    push(&["--target=", unit.target.triple()]);
    // No configuration file next to the program or in the user's directories.
    push(&["--no-default-config"]);
    // The program's path is taken as given: no symbolic link followed out of the view.
    push(&["-no-canonical-prefixes"]);
    // The resource headers (`stddef.h`, `stdarg.h`) from the shelf's clang, named, not derived.
    push(&["-resource-dir=", tools.clang, "/lib/clang/", CLANG_MAJOR]);
    let mut version = [0u8; 12];
    match unit.target {
        Target::MacosArm64 { min_major, min_minor } => {
            push(&["-isysroot"]);
            push(&[tools.sysroot]);
            push(&["-mmacosx-version-min=", decimal_pair(&mut version, min_major, min_minor)]);
        }
        Target::LinuxArm64 | Target::LinuxX64 => push(&["--sysroot=", tools.sysroot]),
    }
    if cxx {
        // The shelf's libc++, found by the driver beside its program (`include/c++/v1`).
        push(&["-stdlib=libc++"]);
        push(&["-fno-modules"]);
    }
    push(&["-x"]);
    push(&[if cxx { "c++" } else { "c" }]);
    push(&["-std=", unit.std]);
    for dir in unit.include {
        push(&["-I", tools.package, slash(*dir), dir.as_str()]);
    }
    // No clock: the macros that would read one are fixed, and their redefinition is not a
    // warning.
    push(&["-Wno-builtin-macro-redefined"]);
    for fixed in FIXED {
        push(&["-D", fixed]);
    }
    for define in unit.defines {
        if !identifier(define.name) || matches!(define.name, "__DATE__" | "__TIME__" | "__TIMESTAMP__") {
            return Err(FlagFault::DefineName);
        }
        match define.value {
            None => push(&["-D", define.name]),
            Some(value) if value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') => {
                return Err(FlagFault::DefineValue);
            }
            Some(value) => push(&["-D", define.name, "=", value]),
        }
    }
    push(&[match unit.opt {
        Opt::Quick => "-O0",
        Opt::Fast => "-O2",
    }]);
    push(&["-fPIC"]);
    // blaze-cunit's requirements: one section a function and a datum, so a code record takes
    // one function; no common symbols, so every tentative definition is a datum of its own.
    // (Mach-O's `.subsections_via_symbols` is what clang writes for Darwin by itself: the
    // ladder asserts it in the object, it is not a flag.)
    push(&["-ffunction-sections"]);
    push(&["-fdata-sections"]);
    push(&["-fno-common"]);
    push(&[match unit.debug {
        Debug::None => "-g0",
        Debug::Lines => "-gline-tables-only",
    }]);
    push(&["-fdebug-compilation-dir=", tools.package]);
    push(&["-fcoverage-compilation-dir=", tools.package]);
    push(&["-gno-record-gcc-switches"]);
    // Diagnostics as rows (the helper's consumer), never a reproducer written anywhere.
    push(&["-fno-color-diagnostics"]);
    push(&["-fmessage-length=0"]);
    push(&["-fno-crash-diagnostics"]);
    match mode {
        Mode::Decls => push(&["-fsyntax-only"]),
        // The object is emitted to memory by the helper; no `-o`, no file is written.
        Mode::Compile => push(&["-c"]),
    }
    match unit.source {
        Source::File(file) => push(&[tools.package, slash(file), file.as_str()]),
        Source::Headers(headers) => {
            for header in headers {
                push(&["-include"]);
                push(&[tools.package, slash(*header), header.as_str()]);
            }
            push(&[IMPORT_MAIN]);
        }
    }
    Ok(a)
}

/// `/` before a relative path that is not the root itself.
fn slash(path: RelPath<'_>) -> &'static str {
    if path.is_root() { "" } else { "/" }
}

/// `/` alone, or `/` and segments joined by single `/`, none `.` or `..`, no trailing `/`.
fn canonical_absolute(spelling: &str) -> bool {
    let Some(rest) = spelling.strip_prefix('/') else { return false };
    rest.is_empty() || rest.split('/').all(|s| !matches!(s, "" | "." | "..") && !s.contains('\0'))
}

fn identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

/// `major.minor` written into `buf`.
fn decimal_pair(buf: &mut [u8; 12], major: u16, minor: u16) -> &str {
    let mut at = 0;
    for (i, n) in [major, minor].into_iter().enumerate() {
        if i == 1 {
            buf[at] = b'.';
            at += 1;
        }
        let mut digits = [0u8; 5];
        let mut len = 0;
        let mut n = n;
        loop {
            digits[len] = b"0123456789"[usize::from(n % 10)];
            len += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for d in digits[..len].iter().rev() {
            buf[at] = *d;
            at += 1;
        }
    }
    // Digits and a dot alone were written.
    core::str::from_utf8(&buf[..at]).unwrap_or("")
}
