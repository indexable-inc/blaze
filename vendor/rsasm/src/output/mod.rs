//! Output writers.

pub mod coff;
pub mod elf;
pub mod ihex;
pub mod macho;
pub mod raw;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Format {
    /// ELF relocatable object.
    Elf,
    /// PE/COFF relocatable object, for Windows.
    Coff,
    /// Mach-O relocatable object (`MH_OBJECT`).
    MachO,
    /// A flat image of the allocatable sections, with no metadata.
    Binary,
    /// The flat image as Intel HEX records; see [`ihex`].
    IntelHex,
}

impl Format {
    /// Whether the output is an image the assembler lays out itself, with
    /// every address final and nothing left to a linker.
    pub fn is_flat(self) -> bool {
        matches!(self, Format::Binary | Format::IntelHex)
    }

    pub fn from_name(s: &str) -> Option<Format> {
        Some(match s {
            "elf" | "elf32" | "elf64" | "o" | "obj" => Format::Elf,
            // `win64` and `win32` are NASM's names for a COFF object, and
            // name the machine as well; see `main`.
            "coff" | "pe" | "win" | "win64" | "win32" => Format::Coff,
            "macho" | "macho64" | "mach-o" => Format::MachO,
            "bin" | "binary" | "raw" => Format::Binary,
            "ihex" | "hex" | "intel-hex" => Format::IntelHex,
            _ => return None,
        })
    }

    /// Whether the format keeps relocation addends in the bytes they
    /// relocate and names its sections and symbols COFF's way.
    pub(crate) fn is_coff(self) -> bool {
        self == Format::Coff
    }

    /// The format a target name asks for: Darwin's triples
    /// (`x86_64-apple-macos`, `arm64-apple-ios`, `*-darwin*`) name Mach-O,
    /// Windows's (`x86_64-pc-windows-msvc`, `*-w64-mingw32`) PE/COFF, and
    /// everything else ELF.
    pub fn for_target(name: &str) -> Format {
        let name = name.to_ascii_lowercase();
        if name.contains("apple") || name.contains("darwin") || name.contains("macos") {
            Format::MachO
        } else if name.contains("windows") || name.contains("mingw") || name.contains("cygwin") {
            Format::Coff
        } else {
            Format::Elf
        }
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum OutputError {
    Unsupported(String),
    Io(std::io::Error),
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputError::Unsupported(m) => write!(f, "{m}"),
            OutputError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OutputError {}

impl From<std::io::Error> for OutputError {
    fn from(e: std::io::Error) -> OutputError {
        OutputError::Io(e)
    }
}
