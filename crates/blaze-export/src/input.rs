//! What an export is given: the program's reached functions as code records, its data (statics
//! and vtables), how it starts, its panic strategy and, when there is one, the profile earlier
//! runs left. Everything here is borrowed: the caller holds the records (read in place from the
//! store) and the export keeps nothing.

use blaze_code::record::{CodeView, Symbol, Target};
use blaze_native::image::Image;

/// The tier a function's record was made at. The frame (bottom.md, section 11) says an export is
/// "at the optimized tier"; until that tier makes records on the new layer the export takes the
/// records it is given and says which tier each is at ([`crate::Exported::tiers`]), never more.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Tier {
    /// blaze-emit's baseline: what a run makes first.
    Fast,
    /// The background tier, optimizing across functions.
    Optimized,
}

/// One reached function instance: its symbol (blaze-program's, by item and type arguments), its
/// sealed record read in place, and the tier that record was made at.
#[derive(Clone, Copy)]
pub struct Function<'r> {
    pub symbol: Symbol,
    pub record: CodeView<'r>,
    pub tier: Tier,
}

/// Where a datum lives once the program has started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataPlace {
    /// Never written after the loader's fixups (a vtable, an immutable static without interior
    /// mutability): in the region the loader makes read-only (`__DATA_CONST`, `PT_GNU_RELRO`).
    ReadOnly,
    /// Written by the program (`static mut`, a static with interior mutability).
    Writable,
    /// Writable, every byte zero at start, `len` bytes: no bytes in the file (`__bss`, an ELF
    /// load's memory past its file bytes). Its `bytes` and `words` are empty.
    Zeroed { len: u64 },
}

/// An address-holding word of a datum: at byte `at` (8-aligned), the address of `target` plus
/// `addend`, or, with `target: None`, of this datum's own byte `addend`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DataWord {
    pub at: u32,
    pub target: Option<Target>,
    pub addend: i64,
}

/// A static, vtable, or thread-local template, by the target the records name it with
/// ([`Space::Static`], [`Space::Vtable`], or [`Space::Tls`]), with its first bytes and the words
/// in them the loader fills. For [`Space::Tls`], `bytes` (under `ReadOnly` or `Writable`) are the
/// initial template copied into each thread's allocation on first touch, and `Zeroed { len }`
/// describes a zero-initialized thread-local with no file bytes.
///
/// [`Space::Static`]: blaze_code::record::Space::Static
/// [`Space::Vtable`]: blaze_code::record::Space::Vtable
/// [`Space::Tls`]: blaze_code::record::Space::Tls
#[derive(Clone, Copy)]
pub struct Data<'r> {
    pub target: Target,
    pub bytes: &'r [u8],
    /// A power of two, at most a page.
    pub align: u32,
    /// Sorted by `at`, not overlapping.
    pub words: &'r [DataWord],
    /// C symbol names for any `Space::Outside` targets named by `words`.
    pub outside: &'r [(Symbol, &'r str)],
    pub place: DataPlace,
}

/// How the program is entered, from `main`'s signature (blaze-ladder run.rs `entry_shape` reads
/// the same three forms for a run).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    /// `fn main()` with the Rust ABI, called as a run calls it today: no `lang_start`. The start
    /// stub calls it and returns 0.
    RustMain { main: Symbol },
    /// `extern "C" fn main() -> i32` under `#![no_main]`: its value is the exit status.
    CMain { main: Symbol },
    /// `main` through the library's `#[lang = "start"]` function
    /// (`lang_start::<T>(main: fn() -> T, argc: isize, argv: *const *const u8, sigpipe: u8) ->
    /// isize`, the pinned library's library/std/src/rt.rs), when the program's reach includes it.
    LangStart { lang_start: Symbol, main: Symbol },
    /// bstd's hosted entry (`bstd_hosted_start(*const HostTable) -> i32`): its table is the
    /// launcher's, which an exported file does not have yet. Refused by name.
    Hosted { start: Symbol },
}

/// The program's panic strategy, from its profile (`panic = "unwind"` is Rust's default).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panic {
    Unwind,
    Abort,
}

/// What earlier runs observed (run.md, "Export takes what runs observed"). An input like any
/// other: its digest is part of the exported file's identity, and the export says which profile
/// it used. Today it orders functions only: `hot` first, in the order given, then the rest by
/// the call graph. Nothing fills it yet; `None` exports without one.
#[derive(Clone, Copy)]
pub struct Profile<'p> {
    /// The merged profile record's digest.
    pub digest: [u8; 32],
    /// Functions the runs saw, hottest first. A symbol not in the program is ignored by the
    /// order, never an error: a profile outlives the code it saw.
    pub hot: &'p [Symbol],
}

/// The file form written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    /// Mach-O `MH_EXECUTE`, position-independent, arm64, ad-hoc signed.
    MacosArm64,
    /// ELF `ET_DYN` (PIE), AArch64, glibc's dynamic loader.
    LinuxArm64,
    /// WebAssembly 64-bit linear memory (`memory64`, Wasmtime 64) over Blaze's sans-IO ring ABI.
    Wasm64,
}

impl Platform {
    /// The page a layout aligns its regions to: 16 KiB on macOS on arm64; on Linux and Wasm64
    /// 64 KiB (arm64 max kernel page / WebAssembly page size).
    pub fn page(self) -> u64 {
        match self {
            Platform::MacosArm64 => 0x4000,
            Platform::LinuxArm64 | Platform::Wasm64 => 0x1_0000,
        }
    }
}

/// A C name the program exports (`#[no_mangle]`: the identifier; `#[export_name = ".."]`: the
/// string; Reference, abi), without a platform's prefix, and the function it names. The native
/// image's words (C calling Rust) are found by it (native_merge.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CExport<'r> {
    pub name: &'r [u8],
    pub symbol: Symbol,
}

/// The program's thread-local runtime symbols (`blaze_code::tls`, THREADS.md D2), when the
/// program has Rust thread-locals ([`Space::Tls`]): `descriptor` is the `Space::Static` symbol
/// the code records name for the program-wide 40-byte `Desc` (which `blaze-export` synthesizes
/// in writable data, together with its `VarRow` table, index words and startup initializer);
/// `ender` is the `pthread_key_create` destructor function among `functions`.
///
/// [`Space::Tls`]: blaze_code::record::Space::Tls
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TlsRuntime {
    pub descriptor: Symbol,
    pub ender: Symbol,
}

/// One export's whole input.
#[derive(Clone, Copy)]
pub struct ProgramIn<'r> {
    /// The program's name: the code signature's identifier on macOS. Text from the manifest,
    /// never a path, so the bytes do not depend on where the project is.
    pub name: &'r str,
    /// Every function the program reaches, each once, in any order.
    pub functions: &'r [Function<'r>],
    /// Every static, vtable, and thread-local template a record or a datum names, each once, in
    /// any order.
    pub data: &'r [Data<'r>],
    pub entry: Entry,
    pub panic: Panic,
    /// The program's personality routine (its `eh_personality` lang item: std's
    /// `rust_eh_personality`), one of `functions`: named by the unwind tables' CIE when a row
    /// lands. `None` refuses any landing row by name (`ExportError::NoPersonality`).
    pub personality: Option<Symbol>,
    /// The program's thread-local runtime (`descriptor` and `ender`), when any function or datum
    /// names a Rust thread-local (`Space::Tls`). `None` refuses any `Space::Tls` target or datum
    /// by name (`ExportError::NoTlsRuntime`).
    pub tls: Option<TlsRuntime>,
    pub profile: Option<Profile<'r>>,
    /// The program's native library image (blaze-native `layout::lay`: the archive members its
    /// `#[link]` libraries and build scripts pull), merged into the file (native_merge.rs);
    /// `None`: the program links no C.
    pub natives: Option<&'r Image<'r>>,
    /// Every C name the program exports, sorted by name, each once (two exporters of one name
    /// are refused by name). The front end gives it; only `natives`' words read it.
    pub exports: &'r [CExport<'r>],
}
