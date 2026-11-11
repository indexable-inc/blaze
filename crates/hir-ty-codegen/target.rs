//! What blaze emits code for. The target is an input of each unit, named by its triple
//! (its toolchain's, the same as its crate's `TargetData`), and one process lowers units of
//! several side by side: the host's and wasm32 for proc macros. Everything in the backend that differs between platforms asks the [`Target`] for the property it depends
//! on (architecture, object format, calling convention, unwind tables) and never asks what
//! the daemon itself was built for.

#![forbid(unsafe_code)]

use blaze_stencil::TlsModel;
use cranelift_codegen::{
    ir::{Type, types},
    isa::{self, CallConv, OwnedTargetIsa},
    settings::{self, Configurable, OptLevel},
};
use blaze_core::lock::Lock;

/// Each target a unit has named ([`Target::of`]), parsed once and kept for the process. A
/// process builds for a handful (the host, wasm32 for proc macros), so a scan finds one.
static NAMED: Lock<Vec<&'static Target>> = Lock::new("codegen.target.named", Vec::new());

/// The most targets one process lowers units for. A code generator is kept per target and
/// set of features (`crate::target_of`), by the target's place in [`NAMED`].
pub(crate) const MOST_TARGETS: usize = 8;

/// The most sets of target features one process compiles with: each toolchain's
/// (`Toolchain::features`: a dev job's host features, a release job's baseline) and each
/// `#[target_feature]` set over one of them. A code generator is kept per set
/// (`crate::target_of`).
pub(crate) const MOST_FEATURE_SETS: usize = 128;

const _: () = assert!(MOST_FEATURE_SETS <= u8::MAX as usize + 1);

/// The sets [`Features`] number, past the baseline's (0, never set), in the order they were
/// first named, each once. Only added to, under [`ADDING`], in order, so a slot that is not
/// set yet ends the sets; read with no lock.
#[expect(
    clippy::disallowed_types,
    reason = "the process's one numbering of feature sets: a `Features` is its number, read with no lock"
)]
static FEATURE_SETS: [std::sync::OnceLock<FeatureSet>; MOST_FEATURE_SETS] =
    [const { std::sync::OnceLock::new() }; MOST_FEATURE_SETS];

/// What a [`Features`] numbers: the CPU code is made for and the features past that target's
/// baseline it is made with. Two sets of one list and two CPUs are two numbers, so everything
/// kept by `Features` (a target machine of LLVM's, an ISA of Cranelift's, a function's set) is
/// kept per CPU too.
#[derive(PartialEq, Eq, Debug)]
struct FeatureSet {
    /// `-C target-cpu` resolved (`native` is the host's name), or `None` for the target's own
    /// ([`Target::cpu`]), which every target has another name of.
    cpu: Option<Box<str>>,
    names: Box<[String]>,
}

/// Held while a set is added to [`FEATURE_SETS`].
static ADDING: Lock<()> = Lock::new("codegen.target.feature_sets.adding", ());

/// A CPU and a set of target features past the baseline of [`Target::cpu`] that code is
/// compiled with: `+name` and `-name` in rustc's names, sorted, each once, as
/// `blaze_codegen::Toolchain::features` has them, and the CPU `Toolchain::cpu` names when it is
/// not the target's own ([`Self::cpu`]). Numbered for the process ([`Target::features_of`]), so
/// that a code generator is one per CPU and set and a function's set is a byte. They change the
/// code only: they are in a unit's key through `Toolchain::cpu` and `Toolchain::features` and a
/// function's `#[target_feature]` through its shape, and never in `cfg(target_feature)`, which
/// comes from the flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Features(u8);

impl Features {
    /// None past the baseline.
    pub const BASELINE: Features = Features(0);

    /// The features of the set, as [`Target::features_of`] was given them.
    pub fn names(self) -> &'static [String] {
        match FEATURE_SETS[self.index()].get() {
            Some(set) => &set.names,
            None => &[],
        }
    }

    /// The CPU of the set when it is not its target's own ([`Target::cpu_of`] gives either).
    pub fn cpu(self) -> Option<&'static str> {
        FEATURE_SETS[self.index()].get().and_then(|set| set.cpu.as_deref())
    }

    pub fn is_baseline(self) -> bool {
        self == Features::BASELINE
    }

    pub(crate) fn index(self) -> usize {
        usize::from(self.0)
    }

    /// The number of `cpu` (`None` for the target's own) and `features`, given it is sorted
    /// and each once: a scan of the sets named so far with no lock, and a new number under
    /// [`ADDING`] for a set first named. More than [`MOST_FEATURE_SETS`] is an error naming the
    /// set.
    fn interned(cpu: Option<&str>, features: &[String]) -> Result<Features, String> {
        if cpu.is_none() && features.is_empty() {
            return Ok(Features::BASELINE);
        }
        // The set's number, or the first slot not set yet from `from` on.
        let find = |from: usize| -> Result<Features, usize> {
            for at in from..MOST_FEATURE_SETS {
                match FEATURE_SETS[at].get() {
                    Some(set) if set.cpu.as_deref() == cpu && *set.names == *features => {
                        return Ok(Features(at as u8));
                    }
                    Some(_) => {}
                    None => return Err(at),
                }
            }
            Err(MOST_FEATURE_SETS)
        };
        let unset = match find(1) {
            Ok(found) => return Ok(found),
            Err(unset) => unset,
        };
        let _adding = ADDING.lock();
        let free = match find(unset) {
            Ok(found) => return Ok(found),
            Err(free) => free,
        };
        if free == MOST_FEATURE_SETS {
            // alloc: an error
            return Err(format!(
                "the target features {features:?} of CPU {cpu:?}: one process compiles with at \
                 most {MOST_FEATURE_SETS} sets of a CPU and target features, and this is one more"
            ));
        }
        // alloc: once per set a process compiles with, its CPU's name and features' names
        FEATURE_SETS[free]
            .get_or_init(|| FeatureSet { cpu: cpu.map(Box::from), names: features.into() });
        Ok(Features(free as u8))
    }
}

/// rustc's names of the x86-64 features Cranelift has a setting for, and the setting.
const X86_FEATURES: [(&str, &str); 18] = [
    ("sse3", "has_sse3"),
    ("ssse3", "has_ssse3"),
    ("sse4.1", "has_sse41"),
    ("sse4.2", "has_sse42"),
    ("popcnt", "has_popcnt"),
    ("avx", "has_avx"),
    ("avx2", "has_avx2"),
    ("fma", "has_fma"),
    ("avxvnni", "has_avx_vnni"),
    ("bmi1", "has_bmi1"),
    ("bmi2", "has_bmi2"),
    ("lzcnt", "has_lzcnt"),
    ("avx512f", "has_avx512f"),
    ("avx512vl", "has_avx512vl"),
    ("avx512dq", "has_avx512dq"),
    ("avx512bitalg", "has_avx512bitalg"),
    ("avx512vbmi", "has_avx512vbmi"),
    ("avx512vnni", "has_avx512vnni"),
];

/// rustc's names of the AArch64 features Cranelift has a setting for, and the setting.
const AARCH64_FEATURES: [(&str, &str); 5] = [
    ("lse", "has_lse"),
    ("paca", "has_pauth"),
    ("fp16", "has_fp16"),
    ("dotprod", "has_dotprod"),
    ("i8mm", "has_i8mm"),
];

#[derive(Debug)]
pub struct Target {
    triple: String,
    /// Its place among the targets the process has named ([`Target::of`]).
    index: u8,
    pub arch: Arch,
    pub object_format: ObjectFormat,
    /// The convention of `extern "C"`, which the Rust conventions are laid over.
    pub call_conv: CallConv,
    pub unwind: UnwindFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
    /// WebAssembly with 32-bit addresses.
    Wasm32,
    /// WebAssembly with 64-bit addresses (memory64): blaze's own `wasm64-blaze-wasip1`, the
    /// bootstrap seed's target. An address is an `i64`; the table stays 32-bit, so a function
    /// pointer is an `i64` that `call_indirect` takes wrapped to an `i32`.
    Wasm64,
}

impl Arch {
    /// How long a function is at least, and the byte one that is shorter is filled up with:
    /// the length of a jump from its entry to anywhere a replacement of it may be. Nothing
    /// when every function is that long as it is.
    pub fn shortest_function(self) -> Option<(usize, u8)> {
        match self {
            // `jmp rel32`, filled up with `int3`.
            Arch::X86_64 => Some((5, 0xcc)),
            // `adrp x16`, `add x16` and `br x16`, as `b` reaches only 128 MB, filled up with
            // `udf #0`.
            Arch::Aarch64 => Some((12, 0)),
            // A function of a module is replaced by its slot in the table, not by a jump.
            Arch::Wasm32 | Arch::Wasm64 => None,
        }
    }
}

/// Where the variable arguments of a C function are when it is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableArguments {
    /// In the registers the named parameters left, then on the stack: System V for x86-64
    /// and AArch64.
    Registers,
    /// In the integer registers the named parameters left, which the function writes to the
    /// room the caller keeps below the ones on the stack: Windows x64.
    Homed,
    /// On the stack, eight bytes each, whatever registers are left: Apple's AArch64.
    Stack,
    /// In memory the caller writes them to, each at its own alignment right after the one
    /// before, whose address is one more argument after the named ones: WebAssembly's C.
    Buffer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    Elf,
    MachO,
    Coff,
    /// Relocatable WebAssembly objects.
    Wasm,
}

impl ObjectFormat {
    /// What a label starts with for the assembler to keep it out of the symbols.
    pub(super) fn local_label(self) -> &'static str {
        match self {
            ObjectFormat::MachO => "L",
            ObjectFormat::Elf | ObjectFormat::Coff | ObjectFormat::Wasm => ".L",
        }
    }

    /// Whether a global may be put in a COMDAT. rustc's `TargetOptions::supports_comdat`
    /// (`rustc_target/src/spec/mod.rs`) says no for Darwin and AIX, as LLVM's
    /// `Triple::supportsCOMDAT` does for Mach-O and XCOFF: LLVM ends the process when a module
    /// for Mach-O has one ("MachO doesn't support COMDATs"). Mach-O instead keeps each symbol's
    /// code apart by `.subsections_via_symbols`, which every object LLVM writes for it has.
    pub fn supports_comdat(self) -> bool {
        match self {
            ObjectFormat::Elf | ObjectFormat::Coff | ObjectFormat::Wasm => true,
            ObjectFormat::MachO => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnwindFormat {
    /// DWARF call frame information in `.eh_frame`, landing pads in `.gcc_except_table`.
    EhFrame,
    /// The unwind codes of Windows in `.pdata` and `.xdata`, see `unwind::seh`.
    Seh,
    /// Nothing unwinds: a panic aborts.
    None,
}


impl Target {
    /// The target `triple` names, for one unit: each unit's codegen and layout ask its own,
    /// so units of two targets lower side by side in one process with nothing shared.
    pub fn of(triple: &str) -> Result<&'static Target, String> {
        let mut named = NAMED.lock();
        if let Some(target) = named.iter().find(|it| it.triple == triple) {
            return Ok(target);
        }
        if named.len() >= MOST_TARGETS {
            // alloc: an error
            return Err(format!(
                "{triple} is past the {MOST_TARGETS} targets one process lowers for"
            ));
        }
        let index = named.len() as u8;
        // alloc: leaked once per target named, for the process (setup)
        let target: &'static Target = Box::leak(Box::new(Target::parse(triple, index)?));
        named.push(target);
        Ok(target)
    }

    /// Its place among the targets the process has named, below [`MOST_TARGETS`].
    pub(crate) fn index(&self) -> usize {
        usize::from(self.index)
    }

    /// The set of code for CPU `cpu` (`Toolchain::cpu`, resolved) with the target features past
    /// the baseline of [`Target::cpu`] that `features` names: `-C target-cpu`'s and
    /// `-C target-feature`'s resolved and sorted, as `blaze_codegen::Toolchain::features` has
    /// them (the host's for a dev build that runs where it is built), or such a set with a
    /// function's `#[target_feature]` added. Any number of sets may be named, one for each
    /// toolchain a daemon builds with. A feature that turns off what Cranelift's code of the
    /// target always uses is refused, as are a name that is not `+` or `-` and a feature, a
    /// list that is not sorted with each once, a feature both on and off, and `native`, which
    /// the toolchain resolves to the host's name.
    pub fn features_of(&self, cpu: &str, features: &[String]) -> Result<Features, String> {
        if cpu == "native" || cpu.is_empty() {
            // alloc: an error
            return Err(format!(
                "the CPU {cpu:?} of {}: a toolchain names the CPU it resolved (`native` is the \
                 host's)",
                self.triple
            ));
        }
        if !features.is_sorted_by(|a, b| a < b) {
            // alloc: an error
            return Err(format!("the target features {features:?} are not sorted, each once"));
        }
        for feature in features {
            self.cranelift_setting(feature)?;
            if let Some(name) = feature.strip_prefix('+')
                && features.iter().any(|it| it.strip_prefix('-') == Some(name))
            {
                // alloc: an error
                return Err(format!("the target features {features:?} turn {name} on and off"));
            }
        }
        Features::interned((cpu != self.cpu()).then_some(cpu), features)
    }

    /// The CPU code of `features` is made for: its own ([`Features::cpu`]) or the target's
    /// baseline ([`Self::cpu`]). What LLVM's target machine and each function's `target-cpu`
    /// are told.
    pub fn cpu_of(&self, features: Features) -> &str {
        features.cpu().unwrap_or(self.cpu())
    }

    /// `features` in LLVM's names, as its target machine and a function's `target-features`
    /// are told them (`+name,-name`), after what the target has LLVM told of its own (`+cx16`
    /// on Windows): rustc's `to_llvm_features` for LLVM 23. A function's `target-features`
    /// replaces its target machine's, so it is told the whole of this. Empty for none. Made
    /// once per set of the selected target (`llvm::told`).
    pub fn llvm_features(&self, features: Features) -> String {
        // alloc: per target machine or function told, a few names
        let mut out: Vec<String> = Vec::new();
        // 128-bit atomics are `lock cmpxchg16b`: Windows 8.1 and later require the
        // instruction, and the library calls LLVM makes otherwise (`__atomic_*_16`) are
        // nobody's here.
        if self.arch == Arch::X86_64 && self.object_format == ObjectFormat::Coff {
            // alloc: per target machine or function told, a few names
            out.push("+cx16".to_owned());
        }
        for feature in features.names() {
            let (sign, name) = feature.split_at(1);
            let renamed = renamed_for_llvm(self.arch, name, sign == "+");
            for llvm in renamed.unwrap_or(std::slice::from_ref(&name)) {
                // alloc: per target machine or function told, a few names
                let told = format!("{sign}{llvm}");
                if !out.contains(&told) {
                    out.push(told);
                }
            }
        }
        out.join(",")
    }

    /// The setting of Cranelift's ISA a feature of `-C target-feature` (`+name` or `-name`)
    /// turns on or off, or `None` where Cranelift's choice of instructions does not change:
    /// a feature it has no setting for (it never picks those instructions unasked, and the
    /// IR builder's intrinsics that need one ask for it), or one it has on for every function
    /// already and uses only where the IR asks (`cmpxchg16b`).
    pub(super) fn cranelift_setting(
        &self,
        feature: &str,
    ) -> Result<Option<(&'static str, bool)>, String> {
        let (on, name) = match feature.split_at_checked(1) {
            Some(("+", name)) if !name.is_empty() => (true, name),
            Some(("-", name)) if !name.is_empty() => (false, name),
            // alloc: an error at setup, read once per ISA made.
            _ => {
                return Err(format!("the target feature {feature:?} is not `+` or `-` and a name"));
            }
        };
        let (table, always): (&[(&str, &str)], &[&str]) = match self.arch {
            Arch::X86_64 => (&X86_FEATURES, &["x87", "fxsr", "sse", "sse2", "cmpxchg16b"]),
            Arch::Aarch64 => (&AARCH64_FEATURES, &["neon", "fp-armv8"]),
            Arch::Wasm32 | Arch::Wasm64 => (&[], &[]),
        };
        if always.contains(&name) {
            // Cranelift's code has no x87, `fxsave` or unasked `cmpxchg16b` instruction, so a
            // CPU without them (`-C target-cpu=generic` on x86-64 has no `fxsr`) changes none.
            if !on && !matches!(name, "cmpxchg16b" | "x87" | "fxsr") {
                // alloc: an error at setup, read once per ISA made.
                return Err(format!(
                    "-{name}: Cranelift's code for {} always uses it",
                    self.triple
                ));
            }
            return Ok(None);
        }
        Ok(table.iter().find(|(it, _)| *it == name).map(|&(_, setting)| (setting, on)))
    }

    /// The triple of what the daemon itself runs on: of the program blaze is, never of a unit
    /// it compiles, which names its own ([`Target::of`]).
    pub fn host() -> &'static str {
        // FIXME: the triples of the hosts that have no code emitted for them yet are missing,
        // and `parse` turns those down by name.
        match (std::env::consts::ARCH, std::env::consts::OS) {
            ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
            ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
            ("aarch64", "macos") => "aarch64-apple-darwin",
            ("x86_64", "macos") => "x86_64-apple-darwin",
            ("x86_64", "windows") => "x86_64-pc-windows-msvc",
            ("aarch64", "windows") => "aarch64-pc-windows-msvc",
            _ => "unknown",
        }
    }

    /// The target of the machine a test runs on, which its compiled code runs on too.
    #[cfg(test)]
    pub(crate) fn for_host() -> &'static Target {
        Target::of(Target::host()).expect("blaze emits code for the host it is tested on")
    }

    /// The target of a fixture's crates (`hir_expand::fixture` gives every crate that is not
    /// wasm x86-64 Linux's tuple and layout), whatever the host: a unit of a fixture's function
    /// names it, as codegen refuses a unit whose target is not its crate's. A test that runs
    /// what it compiles uses [`Self::for_host`] and a fixture of the host's layout instead.
    #[cfg(test)]
    pub(crate) fn for_fixtures() -> &'static Target {
        Target::x86_64_linux()
    }

    /// x86-64 Linux, whatever the host: the target of a test that inspects or links code it
    /// never runs, written against System V calls and ELF sections (the LLVM backend's IR
    /// tests).
    #[cfg(test)]
    pub(crate) fn x86_64_linux() -> &'static Target {
        Target::of("x86_64-unknown-linux-gnu").expect("x86-64 Linux's target")
    }

    fn parse(triple: &str, index: u8) -> Result<Target, String> {
        let mut parts = triple.split('-');
        let arch = match parts.next() {
            Some("x86_64") => Arch::X86_64,
            Some("aarch64") => Arch::Aarch64,
            Some("wasm32") => Arch::Wasm32,
            Some("wasm64") => Arch::Wasm64,
            // alloc: an error
            _ => return Err(format!("blaze emits no code for the architecture of {triple}")),
        };
        let system = parts.find(|part| matches!(*part, "linux" | "darwin" | "windows"));
        // Cranelift's names for the C convention: `SystemV` is the psABI's on x86-64 and
        // AAPCS64 on AArch64, which Apple's differs from in what goes on the stack.
        let (object_format, unwind, call_conv) = match (system, arch) {
            // The calling convention is the one of the ISA whose settings are borrowed.
            (_, Arch::Wasm32 | Arch::Wasm64) => {
                (ObjectFormat::Wasm, UnwindFormat::None, CallConv::SystemV)
            }
            (Some("linux"), _) => (ObjectFormat::Elf, UnwindFormat::EhFrame, CallConv::SystemV),
            (Some("darwin"), Arch::Aarch64) => {
                (ObjectFormat::MachO, UnwindFormat::EhFrame, CallConv::AppleAarch64)
            }
            (Some("windows"), Arch::X86_64) if triple.ends_with("-msvc") => {
                (ObjectFormat::Coff, UnwindFormat::Seh, CallConv::WindowsFastcall)
            }
            // FIXME: x86-64 Mach-O and AArch64 PE have no splice written for them, and the
            // GNU flavour of Windows unwinds another way than the tables of `unwind::seh`.
            // alloc: an error
            _ => return Err(format!("blaze emits no code for the system of {triple}")),
        };
        // alloc: once per target parsed, at setup
        Ok(Target { triple: triple.to_owned(), index, arch, object_format, call_conv, unwind })
    }

    pub fn is_wasm(&self) -> bool {
        matches!(self.arch, Arch::Wasm32 | Arch::Wasm64)
    }

    /// Whether its wasm code addresses a 64-bit memory (memory64): `i64` addresses, `i64.const`
    /// for the address of data and a slot, an `i64` stack pointer, and `memory.size`,
    /// `memory.grow`, `memory.copy` and `memory.fill` on `i64`s.
    pub fn memory64(&self) -> bool {
        self.arch == Arch::Wasm64
    }

    /// Whether an object is written as a relocatable wasm object, for a linker: every wasm
    /// target's is.
    pub fn writes_wasm_objects(&self) -> bool {
        self.is_wasm()
    }

    /// The integer an address is.
    pub(super) fn pointer(&self) -> Type {
        match self.pointer_width() {
            target_lexicon::PointerWidth::U16 => types::I16,
            target_lexicon::PointerWidth::U32 => types::I32,
            target_lexicon::PointerWidth::U64 => types::I64,
        }
    }

    /// How wide an address is: the one place a target's address width is decided.
    pub(super) fn pointer_width(&self) -> target_lexicon::PointerWidth {
        match self.arch {
            Arch::X86_64 | Arch::Aarch64 | Arch::Wasm64 => target_lexicon::PointerWidth::U64,
            Arch::Wasm32 => target_lexicon::PointerWidth::U32,
        }
    }

    /// What IR for it is built under (`FunctionBuilder::finalize`, and the signatures
    /// `call_memcpy`, `call_memmove` and `call_memset` declare): `isa`'s calling convention
    /// with the target's own addresses. On wasm `isa` is a native ISA whose settings the IR is
    /// made under, with 64-bit addresses, so its own config would declare wasm32's `memcpy`
    /// as taking and giving an `i64` where every address is an `i32`. This is decided by the
    /// target the function is compiled for, never by how blaze itself was built.
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private
    pub(super) fn frontend_config(&self, isa: &dyn isa::TargetIsa) -> isa::TargetFrontendConfig {
        isa::TargetFrontendConfig { pointer_width: self.pointer_width(), ..isa.frontend_config() }
    }

    /// How many bytes an address takes.
    pub(crate) fn word(&self) -> usize {
        self.pointer().bytes() as usize
    }

    /// Where the second word of a wide pointer is.
    pub(crate) fn meta_at(&self) -> i32 {
        self.word() as i32
    }

    pub fn triple(&self) -> &str {
        &self.triple
    }

    /// The symbol of the `main` the C runtime calls, and whether it takes the count and the
    /// arguments: rustc's `entry-name` and `main-needs-argc-argv` of the target. WASI's
    /// `_start` calls `__main_void`, and std reads the arguments from WASI itself; elsewhere
    /// on wasm a C `main(argc, argv)` is `__main_argc_argv`, as LLVM names the `main` rustc
    /// gives it.
    pub fn entry(&self) -> (&'static str, bool) {
        match self.arch {
            Arch::Wasm32 | Arch::Wasm64 if self.triple.contains("-wasi") => ("__main_void", false),
            Arch::Wasm32 | Arch::Wasm64 => ("__main_argc_argv", true),
            Arch::X86_64 | Arch::Aarch64 => ("main", true),
        }
    }

    /// Whether the program is linked with a C library, for `write` and `abort`: not on a wasm
    /// that is not WASI's, as wasm32-unknown-unknown has nothing to link but the program.
    pub fn has_libc(&self) -> bool {
        !self.is_wasm() || self.triple.contains("-wasi")
    }

    /// Whether a call can name its landing pad and the pad is entered with the exception in a
    /// register, as Cranelift has it for the calling conventions it knows the register of.
    pub fn exception_in_register(&self) -> bool {
        self.call_conv.supports_exceptions()
    }

    /// Where a function that takes `...` finds its variable arguments, for the conventions a
    /// definition of one is compiled for.
    pub fn variable_arguments(&self) -> Option<VariableArguments> {
        match (self.call_conv, self.arch) {
            (_, Arch::Wasm32 | Arch::Wasm64) => Some(VariableArguments::Buffer),
            (CallConv::SystemV, Arch::X86_64 | Arch::Aarch64) => Some(VariableArguments::Registers),
            (CallConv::WindowsFastcall, Arch::X86_64) => Some(VariableArguments::Homed),
            (CallConv::AppleAarch64, Arch::Aarch64) => Some(VariableArguments::Stack),
            _ => None,
        }
    }

    /// Whether code reads the frame pointer, which every function then has to keep.
    fn reads_frame_pointer(&self) -> bool {
        // The pads of a convention Cranelift has no landing pads for take the exception
        // from the frame (see `unwind::seh`), which the frame pointer finds. Variable
        // arguments that are all on the stack are found from it too.
        !self.exception_in_register() || self.variable_arguments() == Some(VariableArguments::Stack)
    }

    /// Whether the target's ABI wants a frame record in every function that calls, whatever
    /// the flags say: Apple's (arm64 and x86-64), as rustc's Apple target specs have
    /// `frame_pointer: NonLeaf`. libunwind's compact `FRAME` mode and every walk by the frame
    /// chain (sampling profilers, `backtrace` without tables) find the caller through it;
    /// a function that calls and keeps no frame record leaves its return address where only
    /// its DWARF rows say. `-C force-frame-pointers=no` cannot take it away, as in rustc
    /// (`FramePointer::ratchet`).
    pub(crate) fn requires_frame_pointers(&self) -> bool {
        self.triple().contains("-apple-")
    }

    /// LLVM's `"frame-pointer"` attribute of a function of this target: `all` when its unit
    /// forces them (`forced`, `-C force-frame-pointers=yes`: its `Toolchain::frame_pointers`),
    /// else `non-leaf` where the ABI wants a frame record in every function that calls
    /// ([`Target::requires_frame_pointers`]), else none, LLVM's default. rustc's
    /// `frame_pointer_type_attr`, for every function blaze gives LLVM, whichever path it
    /// comes by (v1's units and exports have no toolchain and pass `forced: false`).
    pub(crate) fn llvm_frame_pointer(&self, forced: bool) -> Option<&'static str> {
        match (forced, self.requires_frame_pointers()) {
            (true, _) => Some("all"),
            (false, true) => Some("non-leaf"),
            (false, false) => None,
        }
    }

    /// The processor code is made for: the baseline of the target, as rustc compiles for.
    pub fn cpu(&self) -> &'static str {
        match (self.arch, self.object_format) {
            (Arch::X86_64, _) => "x86-64",
            (Arch::Wasm32 | Arch::Wasm64, _) => "generic",
            (Arch::Aarch64, ObjectFormat::MachO) => "apple-m1",
            (Arch::Aarch64, _) => "generic",
        }
    }

    /// What LLVM is told of how a frame larger than a page reaches the stack it needs. A PE
    /// image has `__chkstk` for it, which LLVM calls unasked; elsewhere the code touches
    /// each page itself. Cranelift's code touches each page inline everywhere ([`Self::isa`]).
    pub fn stack_probe(&self) -> Option<&'static str> {
        (self.object_format != ObjectFormat::Coff).then_some("inline-asm")
    }

    /// The section a static with a name gets to itself, where a symbol does not say how
    /// large it is and the section does. Without one a static is as large as the next thing
    /// in its section is far, which counts what that is aligned by. The linker merges
    /// these into the section before the `$`, and the name is not one the writer of the
    /// object would put anything else in.
    pub fn section_of_static(&self, writable: bool) -> Option<&'static str> {
        match self.object_format {
            ObjectFormat::Coff => Some(if writable { ".data$static" } else { ".rdata$static" }),
            ObjectFormat::Elf | ObjectFormat::MachO | ObjectFormat::Wasm => None,
        }
    }

    /// The code generator for the target with `features`. `regalloc` names Cranelift's
    /// register allocator. With `frame_pointers` every function it compiles keeps one, which
    /// a function that asks for its frame pointer needs. `opt` is `None` for code an edit
    /// reaches, `Speed` for what no edit does (the standard library's and dependencies').
    pub(super) fn isa(
        &self,
        features: Features,
        regalloc: &str,
        frame_pointers: bool,
        opt: OptLevel,
    ) -> Result<OwnedTargetIsa, String> {
        fn message(error: impl std::fmt::Display) -> String {
            // alloc: an error at setup, once per ISA made
            error.to_string()
        }
        let mut flags = settings::builder();
        let (pic, tls_model) = self.addressing();
        let tls_model = match tls_model {
            TlsModel::ElfGd => "elf_gd",
            TlsModel::MachO => "macho",
            TlsModel::Coff => "coff",
        };
        flags.set("is_pic", if pic { "true" } else { "false" }).map_err(message)?;
        flags.set("tls_model", tls_model).map_err(message)?;
        // A frame larger than a page touches each page on its way down, as rustc's code does
        // (`probe-stack=inline-asm`, [`Target::stack_probe`]). Windows commits a stack one
        // page at a time as the guard page below the last is touched; elsewhere a thread's
        // stack ends in a guard page that a frame larger than it would step over, into
        // whatever is mapped below, rather than fault where std says the stack overflowed.
        // WebAssembly's stack is the engine's.
        if !self.is_wasm() {
            flags.set("enable_probestack", "true").map_err(message)?;
            flags.set("probestack_strategy", "inline").map_err(message)?;
        }
        if frame_pointers || self.reads_frame_pointer() {
            // Cranelift gives a function its frame pointer only when all of them keep one.
            flags.set("preserve_frame_pointers", "true").map_err(message)?;
        }
        let opt = match opt {
            OptLevel::None => "none",
            OptLevel::Speed => "speed",
            OptLevel::SpeedAndSize => "speed_and_size",
        };
        flags.set("opt_level", opt).map_err(message)?;
        // Off: every build checks the IR blaze makes itself, before any backend or pass is given
        // it (`crate::verified`). Cranelift's own check would walk it again in `Context::compile`
        // and in passes such as `eliminate_unreachable_code` (`tier::fold_known_branches`).
        flags.set("enable_verifier", "false").map_err(message)?;
        flags.set("enable_llvm_abi_extensions", "true").map_err(message)?;
        flags.set("regalloc_algorithm", regalloc).map_err(message)?;
        // Cranelift has no ISA for WebAssembly: its code is written from the IR, and the
        // settings of this one are what the IR is made under.
        let triple = match self.arch {
            Arch::Wasm32 | Arch::Wasm64 => "x86_64-unknown-linux-gnu",
            Arch::X86_64 | Arch::Aarch64 => &self.triple,
        };
        let mut isa = isa::lookup_by_name(triple).map_err(message)?;
        if self.arch == Arch::X86_64 {
            // Only a function of `#[target_feature(enable = "cmpxchg16b")]` compares and
            // swaps 128 bits, as `core::arch::x86_64::cmpxchg16b` does.
            isa.enable("has_cmpxchg16b").map_err(message)?;
        }
        for setting in self.baseline_settings() {
            isa.enable(setting).map_err(message)?;
        }
        for feature in features.names() {
            if let Some((setting, on)) = self.cranelift_setting(feature)? {
                let value = if on { "true" } else { "false" };
                isa.set(setting, value).map_err(message)?;
            }
        }
        isa.finish(settings::Flags::new(flags)).map_err(message)
    }

    /// Whether code is position-independent and how it finds a thread-local: Cranelift's
    /// `is_pic` and `tls_model` ([`Self::isa`]), and the stencil backend's ([`Self::stencil`]).
    fn addressing(&self) -> (bool, TlsModel) {
        // An image of PE reaches all of itself relative to the instruction, and what it
        // imports through the stubs the linker adds: there is no table of addresses to go
        // through.
        match self.object_format {
            ObjectFormat::Elf | ObjectFormat::Wasm => (true, TlsModel::ElfGd),
            ObjectFormat::MachO => (true, TlsModel::MachO),
            ObjectFormat::Coff => (false, TlsModel::Coff),
        }
    }

    /// The target as the stencil backend is told it (`blaze_stencil::Target`): each field
    /// what [`Self::isa`] gives Cranelift. `None` for wasm, which has no stencils.
    pub fn stencil(&self) -> Option<blaze_stencil::Target> {
        let arch = match self.arch {
            Arch::X86_64 => blaze_codegen::Arch::X86_64,
            Arch::Aarch64 => blaze_codegen::Arch::Aarch64,
            Arch::Wasm32 | Arch::Wasm64 => return None,
        };
        let os = match self.object_format {
            ObjectFormat::Elf => blaze_stencil::Os::Linux,
            ObjectFormat::MachO => blaze_stencil::Os::MacOs,
            ObjectFormat::Coff => blaze_stencil::Os::Windows,
            ObjectFormat::Wasm => return None,
        };
        let cpu = match self.cpu() {
            "apple-m1" => blaze_stencil::Cpu::AppleM1,
            _ => blaze_stencil::Cpu::Baseline,
        };
        let (pic, tls) = self.addressing();
        // `isa` leaves Cranelift's `probestack_size_log2` at its default: a page of 4 KiB.
        // Code counts nothing unless its unit's spec says it does (`Spec::counted`).
        Some(blaze_stencil::Target {
            arch,
            os,
            cpu,
            pic,
            tls,
            probe_stack: true,
            probe_log2: 12,
            counts: false,
        })
    }

    /// The settings of Cranelift's ISA that the [`cpu`](Self::cpu) rustc compiles for has:
    /// what every machine the target runs on has, so code may use it unasked. A unit's key
    /// has the CPU. `apple-m1` has LSE: an atomic is one `ldaddal` or `casal` instead of a
    /// loop of exclusive loads and stores, as LLVM's code of it is.
    fn baseline_settings(&self) -> &'static [&'static str] {
        match self.cpu() {
            "apple-m1" => &["has_lse"],
            _ => &[],
        }
    }
}

/// LLVM's names of rustc's target feature `name` of `arch`, turned on or off (`on`), as
/// rustc's `to_llvm_features` gives them for LLVM 23: what LLVM spells otherwise, what rustc
/// ties to it (`neon` and `fp-armv8` both ways; `sse4.2` turns `crc32` on), and nothing for
/// a feature this LLVM no longer has. `None` where LLVM spells it as rustc does.
pub(crate) fn renamed_for_llvm(
    arch: Arch,
    name: &str,
    on: bool,
) -> Option<&'static [&'static str]> {
    Some(match arch {
        Arch::X86_64 => match name {
            "sse4.2" if on => &["sse4.2", "crc32"],
            "pclmulqdq" => &["pclmul"],
            "rdrand" => &["rdrnd"],
            "bmi1" => &["bmi"],
            "cmpxchg16b" => &["cx16"],
            "lahfsahf" => &["sahf"],
            "apxf" => &["egpr", "push2pop2", "ppx", "ndd", "ccmp", "cf", "nf", "zu"],
            _ => return None,
        },
        Arch::Aarch64 => match name {
            "rcpc2" => &["rcpc-immo"],
            "dpb" => &["ccpp"],
            "dpb2" => &["ccdp"],
            "frintts" => &["fptoint"],
            "fcma" => &["complxnum"],
            "pmuv3" => &["perfmon"],
            "paca" | "pacg" => &["pauth"],
            "flagm2" => &["altnzcv"],
            "neon" => &["neon", "fp-armv8"],
            "fhm" => &["fp16fml"],
            "fp16" => &["fullfp16"],
            "fpmr" | "tme" => &[],
            _ => return None,
        },
        Arch::Wasm32 | Arch::Wasm64 => return None,
    })
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::settings::OptLevel;

    use super::{AARCH64_FEATURES, Arch, Features, MOST_FEATURE_SETS, Target, X86_FEATURES};

    fn setting(target: &Target, name: &str) -> Option<bool> {
        setting_with(target, Features::BASELINE, name)
    }

    fn setting_with(target: &Target, features: Features, name: &str) -> Option<bool> {
        let isa = target.isa(features, "backtracking", false, OptLevel::None).expect("the ISA");
        isa.isa_flags().iter().find(|it| it.name == name).and_then(|it| it.as_bool())
    }

    fn names(features: &[&str]) -> Vec<String> {
        features.iter().map(|it| (*it).to_owned()).collect()
    }

    #[test]
    fn a_set_is_one_number_and_two_sets_two() {
        let x86 = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        assert_eq!(x86.features_of(x86.cpu(), &[]), Ok(Features::BASELINE));
        let avx2 = x86.features_of(x86.cpu(), &names(&["+avx", "+avx2"])).expect("a set");
        let again = x86.features_of(x86.cpu(), &names(&["+avx", "+avx2"])).expect("a set");
        let ssse3 = x86.features_of(x86.cpu(), &names(&["+ssse3"])).expect("a set");
        assert_eq!(avx2, again);
        assert_ne!(avx2, ssse3);
        assert!(!avx2.is_baseline() && Features::BASELINE.is_baseline());
        assert_eq!(avx2.names(), ["+avx", "+avx2"]);
        assert!(Features::BASELINE.names().is_empty());
    }

    /// rustc's frame pointers: `all` when forced, Apple's `non-leaf` whatever the flags, else
    /// LLVM's default.
    #[test]
    fn frame_pointers_are_forced_or_the_abis() {
        for apple in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
            let apple = Target::of(apple).expect("a target");
            assert!(apple.requires_frame_pointers());
            assert_eq!(apple.llvm_frame_pointer(false), Some("non-leaf"), "{}", apple.triple());
            assert_eq!(apple.llvm_frame_pointer(true), Some("all"));
        }
        for other in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc"] {
            let other = Target::of(other).expect("a target");
            assert!(!other.requires_frame_pointers(), "{}", other.triple());
            assert_eq!(other.llvm_frame_pointer(false), None);
            assert_eq!(other.llvm_frame_pointer(true), Some("all"));
        }
    }

    #[test]
    fn a_set_out_of_order_twice_or_on_and_off_is_refused() {
        let x86 = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        assert!(x86.features_of(x86.cpu(), &names(&["+avx2", "+avx"])).is_err());
        assert!(x86.features_of(x86.cpu(), &names(&["+avx", "+avx"])).is_err());
        assert!(x86.features_of(x86.cpu(), &names(&["+popcnt", "-popcnt"])).is_err());
        assert!(x86.features_of(x86.cpu(), &names(&["-sse2"])).is_err());
        assert!(x86.features_of(x86.cpu(), &names(&["avx2"])).is_err());
    }

    /// Sets are numbered from many threads at once: each set gets one number.
    #[test]
    fn sets_named_at_once_get_one_number_each() {
        let x86 = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        let sets = [["+bmi1"], ["+bmi2"], ["+lzcnt"], ["+fma"]].map(|it| names(&it));
        let numbered: Vec<Vec<Features>> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        sets.iter()
                            .map(|it| x86.features_of(x86.cpu(), it).expect("a set"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            threads.into_iter().map(|it| it.join().expect("a thread")).collect()
        });
        for each in &numbered {
            assert_eq!(each, &numbered[0]);
        }
        for (set, number) in sets.iter().zip(&numbered[0]) {
            assert_eq!(number.names(), &set[..]);
        }
        assert!(numbered[0].iter().all(|it| it.index() < MOST_FEATURE_SETS));
    }

    /// One ISA per set: each has its set's settings and the baseline's has none of them.
    #[test]
    fn each_set_has_its_own_isa() {
        let x86 = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        let avx2 = x86
            .features_of(
                x86.cpu(),
                &names(&["+avx", "+avx2", "+sse3", "+sse4.1", "+sse4.2", "+ssse3"]),
            )
            .expect("a set");
        let ssse3 = x86.features_of(x86.cpu(), &names(&["+sse3", "+ssse3"])).expect("a set");
        assert_eq!(setting_with(&x86, avx2, "has_avx2"), Some(true));
        assert_eq!(setting_with(&x86, avx2, "has_sse41"), Some(true));
        assert_eq!(setting_with(&x86, ssse3, "has_ssse3"), Some(true));
        assert_eq!(setting_with(&x86, ssse3, "has_avx2"), Some(false));
        assert_eq!(setting(&x86, "has_ssse3"), Some(false));
        let arm = Target::of("aarch64-apple-darwin").expect("a target");
        let off = arm.features_of(arm.cpu(), &names(&["-lse"])).expect("a set");
        assert_eq!(setting_with(&arm, off, "has_lse"), Some(false));
        assert_eq!(setting(&arm, "has_lse"), Some(true));
    }

    /// rustc's `to_llvm_features` for LLVM 23, and Windows' `+cx16` first.
    #[test]
    fn llvm_is_told_the_features_in_its_names() {
        let linux = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        assert_eq!(linux.llvm_features(Features::BASELINE), "");
        let set = linux
            .features_of(linux.cpu(), &names(&[
                "+avx2",
                "+bmi1",
                "+lahfsahf",
                "+pclmulqdq",
                "+sse4.2",
                "-cmpxchg16b",
            ]))
            .expect("a set");
        assert_eq!(linux.llvm_features(set), "+avx2,+bmi,+sahf,+pclmul,+sse4.2,+crc32,-cx16");
        let off = linux.features_of(linux.cpu(), &names(&["-sse4.2"])).expect("a set");
        assert_eq!(linux.llvm_features(off), "-sse4.2");
        let windows = Target::of("x86_64-pc-windows-msvc").expect("a target");
        assert_eq!(windows.llvm_features(Features::BASELINE), "+cx16");
        let avx = windows.features_of(windows.cpu(), &names(&["+avx"])).expect("a set");
        assert_eq!(windows.llvm_features(avx), "+cx16,+avx");
        let arm = Target::of("aarch64-unknown-linux-gnu").expect("a target");
        let set = arm
            .features_of(arm.cpu(), &names(&["+fp16", "+paca", "+pacg", "+rcpc2", "+tme"]))
            .expect("a set");
        assert_eq!(arm.llvm_features(set), "+fullfp16,+pauth,+rcpc-immo");
        let neon = arm.features_of(arm.cpu(), &names(&["+fhm", "+neon"])).expect("a set");
        assert_eq!(arm.llvm_features(neon), "+fp16fml,+neon,+fp-armv8");
    }

    #[test]
    fn only_a_cpu_that_has_lse_is_given_it() {
        let apple = Target::of("aarch64-apple-darwin").expect("a target");
        let linux = Target::of("aarch64-unknown-linux-gnu").expect("a target");
        assert_eq!(setting(&apple, "has_lse"), Some(true));
        assert_eq!(setting(&linux, "has_lse"), Some(false));
    }

    #[test]
    fn the_baseline_of_x86_64_is_what_rustc_compiles_for() {
        for triple in ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"] {
            let target = Target::of(triple).expect("a target");
            let later = ["has_sse3", "has_sse41", "has_popcnt", "has_avx", "has_bmi1", "has_lzcnt"];
            for name in later {
                assert_eq!(setting(&target, name), Some(false), "{triple}: {name}");
            }
        }
    }

    /// The stencil backend is told what Cranelift's ISA is set to, field by field.
    #[test]
    fn the_stencil_target_is_what_cranelift_is_given() {
        use cranelift_codegen::settings::TlsModel as Cranelift;
        let triples = [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ];
        for triple in triples {
            let target = Target::of(triple).expect("a target");
            let stencil = target.stencil().expect("a stencil target");
            let isa = target
                .isa(Features::BASELINE, "backtracking", false, OptLevel::None)
                .expect("the ISA");
            let flags = isa.flags();
            assert_eq!(stencil.pic, flags.is_pic(), "{triple}");
            let tls = match flags.tls_model() {
                Cranelift::ElfGd => blaze_stencil::TlsModel::ElfGd,
                Cranelift::Macho => blaze_stencil::TlsModel::MachO,
                Cranelift::Coff => blaze_stencil::TlsModel::Coff,
                other => panic!("{triple}: tls_model {other:?}"),
            };
            assert_eq!(stencil.tls, tls, "{triple}");
            assert!(stencil.probe_stack && flags.enable_probestack(), "{triple}");
            assert_eq!(stencil.probe_log2, flags.probestack_size_log2(), "{triple}");
            let lse = setting(&target, "has_lse") == Some(true);
            assert_eq!(stencil.cpu == blaze_stencil::Cpu::AppleM1, lse, "{triple}");
        }
        let apple = Target::of("aarch64-apple-darwin")
            .expect("a target")
            .stencil()
            .expect("a stencil target");
        assert_eq!(
            (apple.arch, apple.os),
            (blaze_codegen::Arch::Aarch64, blaze_stencil::Os::MacOs)
        );
        let windows = Target::of("x86_64-pc-windows-msvc")
            .expect("a target")
            .stencil()
            .expect("a stencil target");
        assert_eq!(
            (windows.arch, windows.os),
            (blaze_codegen::Arch::X86_64, blaze_stencil::Os::Windows)
        );
        assert_eq!(Target::of("wasm32-wasip1").expect("a target").stencil(), None);
    }

    #[test]
    fn cranelift_checks_nothing_blazes_verifier_checked() {
        let target = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        let isa =
            target.isa(Features::BASELINE, "backtracking", false, OptLevel::None).expect("the ISA");
        assert!(!isa.flags().enable_verifier());
        assert_eq!(isa.flags().opt_level(), OptLevel::None);
        let isa = target
            .isa(Features::BASELINE, "backtracking", false, OptLevel::Speed)
            .expect("the ISA");
        assert_eq!(isa.flags().opt_level(), OptLevel::Speed);
    }

    /// Every setting a feature names is one Cranelift's ISA of the architecture has.
    #[test]
    fn every_mapped_feature_is_a_setting_of_its_isa() {
        let x86 = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        let arm = Target::of("aarch64-unknown-linux-gnu").expect("a target");
        for (target, table) in [(&x86, &X86_FEATURES[..]), (&arm, &AARCH64_FEATURES[..])] {
            for &(feature, name) in table {
                assert_eq!(setting(target, name).map(|_| ()), Some(()), "{feature}: {name}");
                let plus = format!("+{feature}");
                assert_eq!(target.cranelift_setting(&plus), Ok(Some((name, true))));
                let minus = format!("-{feature}");
                assert_eq!(target.cranelift_setting(&minus), Ok(Some((name, false))));
            }
        }
    }

    /// `f16` and `f128` are lowered on AArch64 under Apple's convention as under System V's,
    /// and Cranelift's `apple_aarch64` passes and returns them; Windows' x64 refuses them.
    #[test]
    fn apple_aarch64_passes_f16_and_f128() {
        use cranelift_codegen::ir::{self, AbiParam, InstBuilder as _, types};
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
        let apple = Target::of("aarch64-apple-darwin").expect("a target");
        assert_eq!(apple.call_conv, cranelift_codegen::isa::CallConv::AppleAarch64);
        assert!(crate::f16_f128::soft_floats(&apple));
        assert!(crate::f16_f128::soft_floats(
            &Target::of("aarch64-unknown-linux-gnu").expect("a target")
        ));
        assert!(!crate::f16_f128::soft_floats(
            &Target::of("x86_64-pc-windows-msvc").expect("a target")
        ));
        let isa =
            apple.isa(Features::BASELINE, "backtracking", false, OptLevel::None).expect("the ISA");
        let mut signature = ir::Signature::new(apple.call_conv);
        signature.params.extend([AbiParam::new(types::F16), AbiParam::new(types::F128)]);
        signature.returns.extend([AbiParam::new(types::F128), AbiParam::new(types::F16)]);
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), signature);
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut context);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        builder.seal_block(block);
        let [half, quad] = builder.block_params(block) else { panic!("two parameters") };
        let (half, quad) = (*half, *quad);
        builder.ins().return_(&[quad, half]);
        builder.finalize(isa.frontend_config());
        let mut compiled = cranelift_codegen::Context::for_function(func);
        if let Err(e) = compiled.compile(&*isa, &mut Default::default()) {
            panic!("{:?}", e.inner);
        }
    }

    #[test]
    fn features_cranelift_always_uses_cannot_be_turned_off() {
        let x86 = Target::of("x86_64-unknown-linux-gnu").expect("a target");
        assert_eq!(x86.cranelift_setting("+sse2"), Ok(None));
        assert!(x86.cranelift_setting("-sse2").is_err());
        assert_eq!(x86.cranelift_setting("-cmpxchg16b"), Ok(None));
        assert_eq!(x86.cranelift_setting("+aes"), Ok(None));
        let arm = Target::of("aarch64-apple-darwin").expect("a target");
        assert_eq!(arm.arch, Arch::Aarch64);
        assert!(arm.cranelift_setting("-neon").is_err());
        for malformed in ["", "+", "-", "avx2", "*avx2"] {
            assert!(x86.cranelift_setting(malformed).is_err(), "{malformed:?}");
        }
    }

    /// A target is what its triple says whichever targets the process named before: its word,
    /// its pointer type and its ISA are its own, and naming it again gives the same one.
    #[hegel::test]
    fn a_target_is_its_triple_whatever_was_named_before(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let triples = vec![Target::host(), "wasm32-wasip1", "wasm64-blaze-wasip1"];
        let order: Vec<&str> = tc.draw(gs::vecs(gs::sampled_from(triples)).min_size(1).max_size(8));
        for triple in order {
            let target = Target::of(triple).expect("a target");
            assert_eq!(target.triple(), triple);
            assert!(std::ptr::eq(target, Target::of(triple).expect("a target")));
            let word = match target.arch {
                Arch::Wasm32 => 4,
                Arch::X86_64 | Arch::Aarch64 | Arch::Wasm64 => 8,
            };
            assert_eq!(target.word(), word);
            assert_eq!(target.pointer().bytes() as usize, word);
            // A wasm32 target borrows a native ISA's settings, so only a native one's
            // pointer is the ISA's.
            let isa = crate::isa_for(target).expect("the target's ISA");
            assert!(std::ptr::eq(&**isa, &**crate::isa_for(target).expect("the target's ISA")));
            if !target.is_wasm() {
                assert_eq!(isa.pointer_type(), target.pointer());
            }
        }
    }
}
