//! The library table (NATIVE.md 3.5, M5): which shared library of the platform a name in
//! `#[link(name = ..)]` or a build script's `rustc-link-lib` stands for, as the install name the
//! native image's load command carries. Sans-IO: a fixed table, no search of the host, so the
//! image's bytes depend on no machine's library contents (rule 25).
//!
//! The Rust Reference (items/external-blocks.html): "dylib — Indicates a dynamic library. This is
//! the default if kind is not specified."; "framework — Indicates a macOS framework. This is only
//! valid for macOS targets." A name outside the table is refused by name (`Unknown`), never
//! guessed as `lib<name>.dylib`; `stdc++` on Linux waits on the owner (`Undecided`).

use blaze_own::AString;

use crate::Platform;

/// How a library is named: a plain shared library (`dylib`, the default kind) or a macOS
/// framework.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum LibraryKind {
    Dylib,
    Framework,
}

/// One library a crate of the program declares, by its name as written (no `lib` prefix, no
/// suffix).
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct Library<'a> {
    pub kind: LibraryKind,
    pub name: &'a str,
}

/// Why a library name has no install name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LibraryError<'a> {
    /// The name is not in the platform's table (NATIVE.md 3.5): nothing guesses a path for it.
    Unknown { platform: Platform, kind: LibraryKind, name: &'a str },
    /// `framework=` on a platform other than macOS (Reference: "only valid for macOS targets").
    FrameworkOffMac { name: &'a str },
    /// A name whose library is an open question of the owner's, quoted in `question`.
    Undecided { name: &'a str, question: &'static str },
}

/// The install name of a library: a fixed path (macOS) or soname (Linux), or a framework's path
/// built from its name: `/System/Library/Frameworks/<N>.framework/Versions/A/<N>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallName {
    Fixed(&'static str),
    Framework,
}

/// The question `stdc++` on Linux waits on (NATIVE.md 3.5).
const STDCXX_QUESTION: &str = "toolchain-c.md ships a static libc++, but rocksdb asks for `stdc++`: link the system \
     libstdc++.so.6, or map `stdc++` to the shipped libc++? (owner's decision, NATIVE.md 3.5)";

/// macOS's table: the name as written, then the install name (NATIVE.md 3.5).
const MACOS: &[(&str, &str)] = &[
    ("c", "/usr/lib/libSystem.B.dylib"),
    ("m", "/usr/lib/libSystem.B.dylib"),
    ("pthread", "/usr/lib/libSystem.B.dylib"),
    ("dl", "/usr/lib/libSystem.B.dylib"),
    ("System", "/usr/lib/libSystem.B.dylib"),
    ("info", "/usr/lib/libSystem.B.dylib"),
    ("c++", "/usr/lib/libc++.1.dylib"),
    ("c++abi", "/usr/lib/libc++abi.dylib"),
    ("objc", "/usr/lib/libobjc.A.dylib"),
    ("iconv", "/usr/lib/libiconv.2.dylib"),
    ("z", "/usr/lib/libz.1.dylib"),
    ("resolv", "/usr/lib/libresolv.9.dylib"),
    ("bz2", "/usr/lib/libbz2.1.0.dylib"),
    ("sqlite3", "/usr/lib/libsqlite3.dylib"),
];

/// Linux's (glibc's) table: the name as written, then the soname (NATIVE.md 3.5).
const LINUX: &[(&str, &str)] = &[
    ("c", "libc.so.6"),
    ("pthread", "libc.so.6"),
    ("dl", "libc.so.6"),
    ("rt", "libc.so.6"),
    ("util", "libc.so.6"),
    ("m", "libm.so.6"),
    ("gcc_s", "libgcc_s.so.1"),
];

const FRAMEWORKS: &str = "/System/Library/Frameworks/";
const FRAMEWORK_VERSION: &str = ".framework/Versions/A/";

/// A framework's name is `[A-Za-z0-9_]+` (NATIVE.md 3.5); anything else (a `/`, a `..`, an
/// empty name) would make the path name another file.
fn framework_name_ok(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn find(table: &[(&str, &'static str)], name: &str) -> Option<&'static str> {
    table.iter().find(|(written, _)| *written == name).map(|(_, path)| *path)
}

/// The install name of `library` on `platform`, or why it has none.
pub fn install_name(platform: Platform, library: Library<'_>) -> Result<InstallName, LibraryError<'_>> {
    let unknown = LibraryError::Unknown { platform, kind: library.kind, name: library.name };
    match (platform, library.kind) {
        (Platform::MacosArm64, LibraryKind::Dylib) => find(MACOS, library.name).map(InstallName::Fixed).ok_or(unknown),
        (Platform::MacosArm64, LibraryKind::Framework) => {
            if framework_name_ok(library.name) { Ok(InstallName::Framework) } else { Err(unknown) }
        }
        (Platform::LinuxArm64, LibraryKind::Framework) => Err(LibraryError::FrameworkOffMac { name: library.name }),
        (Platform::LinuxArm64, LibraryKind::Dylib) => {
            if library.name == "stdc++" {
                return Err(LibraryError::Undecided { name: library.name, question: STDCXX_QUESTION });
            }
            find(LINUX, library.name).map(InstallName::Fixed).ok_or(unknown)
        }
    }
}

/// Appends the install name of `library` on `platform` to `out` (the text a load command holds).
/// The verbatim error type has no allocation variant, so growth goes through `push_str`, which
/// takes blaze-own's path for a failed allocation.
pub fn write_install_name<'a>(platform: Platform, library: Library<'a>, out: &mut AString<'_>) -> Result<(), LibraryError<'a>> {
    match install_name(platform, library)? {
        InstallName::Fixed(path) => out.push_str(path),
        InstallName::Framework => {
            out.push_str(FRAMEWORKS);
            out.push_str(library.name);
            out.push_str(FRAMEWORK_VERSION);
            out.push_str(library.name);
        }
    }
    Ok(())
}
