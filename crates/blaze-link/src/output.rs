//! What an image is linked as: its format, target and kind, and its native inputs.

use blaze_codegen::Arch;
use blaze_ids::ContentId;

/// The object format of an image.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Format {
    /// Linux and the other ELF systems.
    Elf,
    /// macOS.
    MachO,
    /// Windows.
    Pe,
    /// WebAssembly (WASI or none).
    Wasm,
}

/// What an image is linked as.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: setup: once per image and its writer, and per linker script read
pub struct Output {
    pub format: Format,
    pub arch: Arch,
    pub kind: OutputKind,
    /// ELF: the program interpreter (`/lib64/ld-linux-x86-64.so.2`); none for a static or
    /// shared image.
    pub interpreter: Option<String>,
    /// Mach-O: `LC_BUILD_VERSION`'s platform (1 is macOS) and its minimum OS and SDK
    /// versions, as `xxxx.yy.zz` nibbles.
    pub platform: Option<Platform>,
    /// Room to grow is kept in every region, for incremental links and live patches; a
    /// release link keeps none.
    pub incremental: bool,
    pub debug: Debug,
    /// The image takes live patches ([`Live`]).
    pub live: Option<Live>,
    /// The high bits of the tags of the image's effects: each is `tags | n` for `n` below
    /// 2^32, so the caller's other effects need other high bits.
    pub tags: u64,
    /// The stack the main thread gets, where the format says it (PE, wasm).
    pub stack: Option<u64>,
    /// PE: `#![windows_subsystem = "windows"]`, a GUI program with no console (rustc's
    /// `/SUBSYSTEM:WINDOWS`). Other formats have no subsystem.
    pub windows_subsystem: bool,
    /// A dev executable whose start hook overlays its code with blaze's code memory
    /// (`blaze_inproc`); `None` for every other image (`blaze build`, release, shared images).
    pub dev_run: Option<DevRun>,
    /// The target triple the image is for, as [`crate::target::output`] was given it: what
    /// tells apart targets of one format and arch (`wasm32-wasip1` from
    /// `wasm32-unknown-unknown`, `-gnu` from `-musl`), whose C runtimes differ.
    pub triple: String,
    /// The image starts with its jump table cell on the execution log (fastcompile-ident-select
    /// 9): a test binary whose fork server records what each test ran. A process that may arm the
    /// log starts on the log table; one that never arms must not, since publishing never writes
    /// the log table and a logged slot would keep its old target. `false` for every other image.
    pub exec_log: bool,
    /// The sanitizers the image's code was built with (the build's `Policy::sanitize`):
    /// [`crate::target::locate`] puts their runtimes on the link line, and the link adds what
    /// LLVM's passes leave to it (MemorySanitizer's options, AddressSanitizer's redzones and
    /// global descriptors, SanitizerCoverage's init calls). Empty for an uninstrumented image.
    pub sanitize: blaze_codegen::settings::Sanitize,
    /// ELF `DT_RUNPATH` and Mach-O `LC_RPATH` entries, absolute: where the loader finds the
    /// shared runtimes [`crate::target::locate`] put on the link line.
    pub rpaths: Vec<String>,
}

/// A dev executable (`blaze run`, `blaze test`): its start hook, `blaze_inproc_start`, swaps its
/// code segments for views of the daemon's code memory and maps the reservations, so:
///
/// - a pointer to `blaze_inproc_start` is the first initializer (ELF `.preinit_array`, Mach-O
///   `__mod_init_func`, a PE TLS callback in `.CRT$XLAB`), rooted against section GC;
/// - each code segment starts on a page in the file and in memory, shares no page, is never
///   writable and is all in the file; PE's code sections are one run from a 64 KiB RVA, the
///   next section on the next 64 KiB RVA;
/// - no load-time write lands in code (no text relocations, no PE base relocation into code,
///   ELF `DF_BIND_NOW`), so the code memory's bytes are the file's at every load address;
/// - Mach-O is signed ad hoc without the hardened runtime; PE has no CFG.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DevRun {
    /// The reserved address ranges, after the image's other segments in this order, each
    /// page-aligned with no access and no file bytes; 0 is none.
    pub arena: u64,
    pub cells: u64,
    pub fixed: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: setup, as `Output`'s
pub enum OutputKind {
    /// An executable entered at the symbol `entry` (`_start` from the C runtime's
    /// `Scrt1.o`; Mach-O's `main`; PE's `mainCRTStartup`).
    Executable { entry: String },
    /// A shared library (`cdylib`, a proc macro) under `name` (its `DT_SONAME`, install
    /// name or DLL name), exporting `exports`.
    Shared { name: String, exports: Vec<String> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Platform {
    pub platform: u32,
    pub minimum: u32,
    pub sdk: u32,
}

/// How much debug info the image carries.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Debug {
    None,
    /// Line tables and function names: backtraces and breakpoints by line.
    Lines,
}

/// A live image: it has a descriptor ([`blaze_live::patch::Descriptor`]) and patch regions,
/// and its patches reach running programs only through the code memory they run
/// ([`crate::Image::commit_into`], [`crate::Image::roll_back_into`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Live;

/// A native input of the link line: a C object, an archive or a shared library.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Native<'a> {
    /// Its content ID: an input changed is one removed and one added.
    pub id: ContentId,
    /// What errors call it: its path as the link line has it.
    pub name: &'a str,
    /// Shared with the image, which keeps it for as long as the input is on the link line:
    /// never copied.
    pub bytes: &'a std::sync::Arc<[u8]>,
    pub kind: NativeKind,
    /// Its place on the link line: symbols resolve to the first definition in this order,
    /// and an archive's members are taken for what is undefined when it is reached.
    pub order: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeKind {
    Object,
    /// Members taken for undefined symbols, or all of them (`--whole-archive`).
    Archive { whole: bool },
    /// A shared library, a linker script naming them, a `.tbd` stub or an import library;
    /// left out of the image's needs when none of its symbols is used and `as_needed`.
    Shared { as_needed: bool },
}
