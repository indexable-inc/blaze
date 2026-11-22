//! The libraries a program's image loads (NATIVE.md 3.5, M5): which shared library of the
//! platform a name in `#[link(name = ..)]` or a build script's `rustc-link-lib` stands for, as
//! the install name the native image's load command carries. Sans-IO: nothing here reads a file.
//!
//! The Rust Reference (items/external-blocks.html): "dylib — Indicates a dynamic library. This is
//! the default if kind is not specified."; "framework — Indicates a macOS framework. This is only
//! valid for macOS targets."
//!
//! - macOS: the install name is a FACT OF THE SDK, never built from the name. ld64 links `-l<N>`
//!   against `<SDK>/usr/lib/lib<N>.tbd` and `-framework <N>` against
//!   `<SDK>/System/Library/Frameworks/<N>.framework/<N>.tbd`, and writes the stub's
//!   `install-name` into the load command ([`crate::tbd`]). [`source`] names that stub
//!   ([`StubPath`]); the caller reads it through the file system it was handed and
//!   [`StubPath::linked`] takes the install name from its bytes. A library whose stub is missing
//!   (a header-only framework such as `Kernel`) is refused by that read, naming the path asked.
//! - Linux: glibc's sonames, a fixed table; a name outside it is refused by name (`Unknown`),
//!   never guessed as `lib<name>.so`; `stdc++` waits on the owner (`Undecided`).
//!
//! A [`Linked`] is the proof a library was resolved: the resolver and the writers take it, and
//! nothing checks a library's name twice (rule 74).

use core::fmt;

use crate::Platform;
use crate::tbd::{self, TbdError};

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

/// Why a library name has no install name on the platform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LibraryError<'a> {
    /// Linux: the name is not in glibc's table (NATIVE.md 3.5): nothing guesses a soname for it.
    Unknown { platform: Platform, kind: LibraryKind, name: &'a str },
    /// macOS: the name cannot be one file's name in the SDK (empty, or with a `/`, a `\` or a
    /// NUL in it): the stub's path would name another file.
    Name { kind: LibraryKind, name: &'a str },
    /// `framework=` on a platform other than macOS (Reference: "only valid for macOS targets").
    FrameworkOffMac { name: &'a str },
    /// A name whose library is an open question of the owner's, quoted in `question`.
    Undecided { name: &'a str, question: &'static str },
}

/// The question `stdc++` on Linux waits on (NATIVE.md 3.5).
const STDCXX_QUESTION: &str = "toolchain-c.md ships a static libc++, but rocksdb asks for `stdc++`: link the system \
     libstdc++.so.6, or map `stdc++` to the shipped libc++? (owner's decision, NATIVE.md 3.5)";

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

/// Where the SDK keeps the stubs of `-l<N>`, relative to its root.
const MACOS_LIBRARIES: &str = "usr/lib";
/// Where the SDK keeps the stubs of `-framework <N>`, relative to its root.
const MACOS_FRAMEWORKS: &str = "System/Library/Frameworks";

/// A library resolved to the install name its load command carries (a soname on Linux): made
/// only by [`source`] (Linux) or [`StubPath::linked`] (macOS, from the SDK's stub).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Linked<'a> {
    library: Library<'a>,
    platform: Platform,
    install: &'a str,
}

impl<'a> Linked<'a> {
    /// The library as declared.
    pub fn library(&self) -> Library<'a> {
        self.library
    }

    /// The platform it was resolved for.
    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// The install name (macOS) or soname (Linux): the text a load command holds.
    pub fn install(&self) -> &'a str {
        self.install
    }
}

/// Where a library's install name comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source<'a> {
    /// Known without a read (Linux's sonames).
    Linked(Linked<'a>),
    /// Stated by the SDK's stub at this path (macOS).
    Stub(StubPath<'a>),
}

impl<'a> Source<'a> {
    /// The library as declared.
    pub fn library(&self) -> Library<'a> {
        match self {
            Source::Linked(linked) => linked.library,
            Source::Stub(at) => at.library,
        }
    }
}

/// The SDK's text stub of one library: [`StubPath::folder`] under the SDK's root, then
/// [`StubPath::file`] (its `Display`) under that folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StubPath<'a> {
    library: Library<'a>,
}

impl<'a> StubPath<'a> {
    /// The library whose stub this is.
    pub fn library(&self) -> Library<'a> {
        self.library
    }

    /// The folder under the SDK's root: `usr/lib` or `System/Library/Frameworks`.
    pub fn folder(&self) -> &'static str {
        match self.library.kind {
            LibraryKind::Dylib => MACOS_LIBRARIES,
            LibraryKind::Framework => MACOS_FRAMEWORKS,
        }
    }

    /// The stub's path under [`StubPath::folder`]: `lib<N>.tbd` or `<N>.framework/<N>.tbd`.
    pub fn file(&self) -> StubFile<'a> {
        StubFile(self.library)
    }

    /// The library linked as the stub `bytes` states (its install name, borrowed from them).
    pub fn linked(self, bytes: &'a [u8]) -> Result<Linked<'a>, TbdError> {
        let stub = tbd::read(bytes)?;
        Ok(Linked { library: self.library, platform: Platform::MacosArm64, install: stub.install_name })
    }
}

/// A stub's path under its folder, as text ([`StubPath::file`]).
#[derive(Clone, Copy)]
pub struct StubFile<'a>(Library<'a>);

impl fmt::Display for StubFile<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.0.name;
        match self.0.kind {
            LibraryKind::Dylib => write!(f, "lib{name}.tbd"),
            LibraryKind::Framework => write!(f, "{name}.framework/{name}.tbd"),
        }
    }
}

impl fmt::Display for StubPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.folder(), self.file())
    }
}

/// A name that can be one file's name: not empty, no `/`, `\` or NUL, not `.` or `..`.
fn file_name_ok(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

fn find(table: &[(&str, &'static str)], name: &str) -> Option<&'static str> {
    table.iter().find(|(written, _)| *written == name).map(|(_, path)| *path)
}

/// Where the install name of `library` on `platform` comes from, or why it has none.
pub fn source(platform: Platform, library: Library<'_>) -> Result<Source<'_>, LibraryError<'_>> {
    match (platform, library.kind) {
        (Platform::MacosArm64, kind) => {
            if !file_name_ok(library.name) {
                return Err(LibraryError::Name { kind, name: library.name });
            }
            Ok(Source::Stub(StubPath { library }))
        }
        (Platform::LinuxArm64, LibraryKind::Framework) => Err(LibraryError::FrameworkOffMac { name: library.name }),
        (Platform::LinuxArm64, LibraryKind::Dylib) => {
            if library.name == "stdc++" {
                return Err(LibraryError::Undecided { name: library.name, question: STDCXX_QUESTION });
            }
            match find(LINUX, library.name) {
                Some(soname) => Ok(Source::Linked(Linked { library, platform, install: soname })),
                None => Err(LibraryError::Unknown { platform, kind: library.kind, name: library.name }),
            }
        }
    }
}
