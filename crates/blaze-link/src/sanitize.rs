//! What a sanitized image's link makes that LLVM puts in each module beside the functions,
//! which codegen does not give (it reads back each function's own code): MemorySanitizer's
//! option globals, AddressSanitizer's redzones after the image's globals, their
//! `__asan_global` descriptors and the constructor and destructor that register them, and
//! SanitizerCoverage's init calls with each section's bounds. The runtimes themselves are put
//! on the link line by [`crate::target::locate`].
//!
//! Everything here is pieces with fixups, laid out and relocated as any other, so an
//! incremental link keeps them: a relink that adds a static remakes the descriptor array
//! (and the count its constructor reads) with the static's entry, one that removes one
//! remakes it without. The code is the few instructions of each constructor, written here
//! per architecture with its fields, never by a compiler; it has no unwind information, as it
//! neither throws nor is unwound through. Nothing here runs when [`Output::sanitize`] is empty.
//!
//! The descriptors refer to the objects they describe without counting as their users, as
//! LLVM's `-asan-globals-live-support` keeps a descriptor from keeping its global: a
//! datum no code refers to is swept as without AddressSanitizer, and its entry goes with it.
//!
//! [`Output::sanitize`]: crate::Output::sanitize

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_codegen::settings::Sanitizers;
use blaze_codegen::{Arch, CompiledFunction, Init, Static, Unwind};
use blaze_ids::{ContentId, Hasher};

use crate::arena::{FrameVec, collect_in};
use crate::model::{Apply, Class, Fixup, Sym, Writer};
use crate::stage::{Content, SANCOV_COFF, SANCOV_ELF, SANCOV_MACHO, Stage, error};
use crate::state::{ClassId, NewPiece, PieceId, What};
use crate::symbols::SymKey;
use crate::{Change, Format, LinkError};

/// AddressSanitizer's smallest redzone after a global (`getMinRedzoneSizeForGlobal`, its
/// shadow scale's granule rounded up to 32), which is also the alignment it gives every
/// global it instruments.
pub(crate) const MIN_REDZONE: u64 = 32;
/// Its largest (`kMaxRZ`).
pub(crate) const MAX_REDZONE: u64 = 1 << 18;

/// The redzone AddressSanitizer puts after a global of `size` bytes, as LLVM's
/// `getRedzoneSizeForGlobal`: `32 - size` up to 16 bytes, else a quarter of it in 32-byte
/// steps between 32 and 2^18, grown so the global and its redzone end on a 32-byte boundary.
pub(crate) const fn redzone(size: u64) -> u64 {
    if size <= MIN_REDZONE / 2 {
        return MIN_REDZONE - size;
    }
    let quarter = size / MIN_REDZONE / 4 * MIN_REDZONE;
    let zone = if quarter < MIN_REDZONE {
        MIN_REDZONE
    } else if quarter > MAX_REDZONE {
        MAX_REDZONE
    } else {
        quarter
    };
    match size % MIN_REDZONE {
        0 => zone,
        rest => zone + MIN_REDZONE - rest,
    }
}

/// Whether AddressSanitizer instruments a global placed in the section `name` of `format`, as
/// LLVM's `shouldInstrumentGlobal` decides by its section: never the initializer and finalizer
/// arrays or LLVM's own sections; on ELF never a section named as a C identifier (its
/// `__start_`/`__stop_` bounds make it an array); on COFF never a `$`-sorted one; on Mach-O
/// never the Objective-C runtime's, `__cfstring` or a `cstring_literals` section.
pub(crate) fn instruments_section(format: Format, name: &str) -> bool {
    let special = ["llvm.metadata"].contains(&name)
        || name.contains("__llvm")
        || name.contains("__LLVM")
        || [".preinit_array", ".init_array", ".fini_array"].iter().any(|it| name.starts_with(it));
    if special {
        return false;
    }
    match format {
        Format::Elf => !name.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'_'),
        Format::Pe => !name.contains('$'),
        Format::MachO => {
            let mut parts = name.split(',').map(str::trim);
            let (segment, section) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
            let objc = segment == "__OBJC" || (segment == "__DATA" && section.starts_with("__objc_"));
            let cfstring = segment == "__DATA" && section == "__cfstring";
            !objc && !cfstring && parts.next() != Some("cstring_literals")
        }
        Format::Wasm => true,
    }
}

/// The size of a static AddressSanitizer would instrument, as `shouldInstrumentGlobal`
/// decides by the static itself: not a thread-local, not aligned past [`MIN_REDZONE`], not
/// empty, and in a section it instruments.
pub(crate) fn instrumented_static(format: Format, it: &Static) -> Option<u64> {
    let size = match &it.init {
        Init::Bytes { bytes, .. } => bytes.len() as u64,
        Init::Zeroed { size } => *size,
    };
    let section = it.section.as_deref().is_none_or(|name| instruments_section(format, name));
    (!it.thread_local && u64::from(it.align) <= MIN_REDZONE && size > 0 && section).then_some(size)
}

/// The same of a datum: every one but a function's language-specific data area, which is
/// its unwinder's table and no global of the program.
pub(crate) fn instrumented_data(data: &blaze_codegen::Data, lsda: Option<&ContentId>) -> Option<u64> {
    let size = data.bytes.len() as u64;
    (lsda != Some(&data.id) && u64::from(data.align) <= MIN_REDZONE && size > 0).then_some(size)
}

/// A function's language-specific data area, one of its data by id.
pub(crate) fn lsda(function: &CompiledFunction) -> Option<&ContentId> {
    match &function.unwind {
        Some(Unwind::Dwarf { lsda, .. } | Unwind::Windows { lsda, .. }) => lsda.as_ref(),
        None => None,
    }
}

/// A field of a constructor's code: at `at`, `apply` of its target `target` (an index into
/// what the code is written for) plus `addend`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Field {
    pub at: u64,
    pub apply: Apply,
    pub target: u8,
    pub addend: i64,
}

const fn field(at: u64, apply: Apply, target: u8, addend: i64) -> Field {
    Field { at, apply, target, addend }
}

const fn words_le<const N: usize, const B: usize>(words: [u32; N]) -> [u8; B] {
    let mut out = [0; B];
    let mut at = 0;
    while at < N {
        let bytes = words[at].to_le_bytes();
        let mut byte = 0;
        while byte < 4 {
            out[4 * at + byte] = bytes[byte];
            byte += 1;
        }
        at += 1;
    }
    out
}

/// A constructor's code and its fields.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stub {
    pub code: &'static [u8],
    pub fields: &'static [Field],
}

/// The descriptor array's header, the count `n` the constructor passes: the entries follow it.
pub(crate) const HEADER: u64 = 8;
/// One `__asan_global`: eight pointer-sized words.
pub(crate) const GLOBAL: u64 = 64;
/// Its words' offsets: `beg`, `size`, `size_with_redzone`, `name`, `module_name`,
/// `has_dynamic_init`, `source_location`, `odr_indicator`.
pub(crate) const BEG: u64 = 0;
pub(crate) const SIZE: u64 = 8;
pub(crate) const SIZE_WITH_REDZONE: u64 = 16;
pub(crate) const NAME: u64 = 24;
pub(crate) const MODULE_NAME: u64 = 32;
pub(crate) const ODR_INDICATOR: u64 = 56;

/// x86-64 System V: `lea rdi, [rip + D + 8]`, `mov rsi, [rip + D]`, `jmp callee`, where `D`
/// (target 0) is the descriptor array's header and `callee` (target 1)
/// `__asan_register_globals` or `__asan_unregister_globals`, called with the entries and their
/// count. A tail call: the stack is as the loader called the constructor with.
const X86_REGISTER: [u8; 19] = [
    0x48, 0x8d, 0x3d, 0, 0, 0, 0, // lea rdi, [rip + D + 8]
    0x48, 0x8b, 0x35, 0, 0, 0, 0, // mov rsi, [rip + D]
    0xe9, 0, 0, 0, 0, // jmp callee
];
const X86_REGISTER_FIELDS: [Field; 3] = [
    field(3, Apply::Rel32, 0, HEADER as i64 - 4),
    field(10, Apply::Rel32, 0, -4),
    field(15, Apply::X86Branch32, 1, -4),
];

/// AArch64: `adrp x0, D + 8`, `add x0, x0, :lo12:D + 8`, `adrp x1, D`, `ldr x1, [x1, :lo12:D]`,
/// `b callee`.
const A64_REGISTER: [u8; 20] = words_le([0x9000_0000, 0x9100_0000, 0x9000_0001, 0xf940_0021, 0x1400_0000]);
const A64_REGISTER_FIELDS: [Field; 5] = [
    field(0, Apply::A64Page21, 0, HEADER as i64),
    field(4, Apply::A64Lo12 { shift: 0 }, 0, HEADER as i64),
    field(8, Apply::A64Page21, 0, 0),
    field(12, Apply::A64Lo12 { shift: 3 }, 0, 0),
    field(16, Apply::A64Jump26, 1, 0),
];

/// AddressSanitizer's constructor (or destructor) for `arch`: [`X86_REGISTER`],
/// [`A64_REGISTER`].
pub(crate) fn register(arch: Arch) -> Result<Stub, LinkError> {
    match arch {
        Arch::X86_64 => Ok(Stub { code: &X86_REGISTER, fields: &X86_REGISTER_FIELDS }),
        Arch::Aarch64 => Ok(Stub { code: &A64_REGISTER, fields: &A64_REGISTER_FIELDS }),
        Arch::Wasm32 => Err(error(None, "AddressSanitizer on wasm: a module has no constructors to register globals in")),
    }
}

/// x86-64 System V, Mach-O's constructor: registers as [`X86_REGISTER`] does, then
/// `__cxa_atexit(destructor, 0, __dso_handle)` as a tail call, as LLVM registers a module
/// destructor on Darwin, whose dyld runs no `__mod_term_func`. Targets: the descriptor array
/// (0), `__asan_register_globals` (1), the destructor (2), `__dso_handle` (3), `__cxa_atexit`
/// (4).
const X86_AT_EXIT: [u8; 42] = [
    0x50, // push rax
    0x48, 0x8d, 0x3d, 0, 0, 0, 0, // lea rdi, [rip + D + 8]
    0x48, 0x8b, 0x35, 0, 0, 0, 0, // mov rsi, [rip + D]
    0xe8, 0, 0, 0, 0, // call __asan_register_globals
    0x48, 0x8d, 0x3d, 0, 0, 0, 0, // lea rdi, [rip + destructor]
    0x31, 0xf6, // xor esi, esi
    0x48, 0x8d, 0x15, 0, 0, 0, 0, // lea rdx, [rip + __dso_handle]
    0x58, // pop rax
    0xe9, 0, 0, 0, 0, // jmp __cxa_atexit
];
const X86_AT_EXIT_FIELDS: [Field; 6] = [
    field(4, Apply::Rel32, 0, HEADER as i64 - 4),
    field(11, Apply::Rel32, 0, -4),
    field(16, Apply::X86Branch32, 1, -4),
    field(23, Apply::Rel32, 2, -4),
    field(32, Apply::Rel32, 3, -4),
    field(38, Apply::X86Branch32, 4, -4),
];

/// AArch64, Mach-O's constructor: the same as [`X86_AT_EXIT`], in a frame of its own.
const A64_AT_EXIT: [u8; 56] = words_le([
    0xa9bf_7bfd, // stp x29, x30, [sp, #-16]!
    0x9100_03fd, // mov x29, sp
    0x9000_0000, // adrp x0, D + 8
    0x9100_0000, // add x0, x0, :lo12:D + 8
    0x9000_0001, // adrp x1, D
    0xf940_0021, // ldr x1, [x1, :lo12:D]
    0x9400_0000, // bl __asan_register_globals
    0x9000_0000, // adrp x0, destructor
    0x9100_0000, // add x0, x0, :lo12:destructor
    0xd280_0001, // mov x1, #0
    0x9000_0002, // adrp x2, __dso_handle
    0x9100_0042, // add x2, x2, :lo12:__dso_handle
    0xa8c1_7bfd, // ldp x29, x30, [sp], #16
    0x1400_0000, // b __cxa_atexit
]);
const A64_AT_EXIT_FIELDS: [Field; 10] = [
    field(8, Apply::A64Page21, 0, HEADER as i64),
    field(12, Apply::A64Lo12 { shift: 0 }, 0, HEADER as i64),
    field(16, Apply::A64Page21, 0, 0),
    field(20, Apply::A64Lo12 { shift: 3 }, 0, 0),
    field(24, Apply::A64Call26, 1, 0),
    field(28, Apply::A64Page21, 2, 0),
    field(32, Apply::A64Lo12 { shift: 0 }, 2, 0),
    field(40, Apply::A64Page21, 3, 0),
    field(44, Apply::A64Lo12 { shift: 0 }, 3, 0),
    field(52, Apply::A64Jump26, 4, 0),
];

/// Mach-O's AddressSanitizer constructor for `arch`: [`X86_AT_EXIT`], [`A64_AT_EXIT`].
pub(crate) fn at_exit(arch: Arch) -> Result<Stub, LinkError> {
    match arch {
        Arch::X86_64 => Ok(Stub { code: &X86_AT_EXIT, fields: &X86_AT_EXIT_FIELDS }),
        Arch::Aarch64 => Ok(Stub { code: &A64_AT_EXIT, fields: &A64_AT_EXIT_FIELDS }),
        Arch::Wasm32 => Err(error(None, "AddressSanitizer on wasm: a module has no constructors to register globals in")),
    }
}

/// SanitizerCoverage's init constructor: `prologue`, then one `call` per section, then
/// `epilogue`. A call's fields are of the section's start (target 0), its end (1) and the
/// init function (2), which it calls with them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Calls {
    pub prologue: &'static [u8],
    pub call: &'static [u8],
    pub fields: &'static [Field],
    pub epilogue: &'static [u8],
}

/// x86-64 System V: `push rax` aligns the stack for the calls; each is `lea rdi, [rip +
/// start]`, `lea rsi, [rip + end]`, `call init`; then `pop rax`, `ret`.
const X86_CALLS: Calls = Calls {
    prologue: &[0x50],
    call: &[
        0x48, 0x8d, 0x3d, 0, 0, 0, 0, // lea rdi, [rip + start]
        0x48, 0x8d, 0x35, 0, 0, 0, 0, // lea rsi, [rip + end]
        0xe8, 0, 0, 0, 0, // call init
    ],
    fields: &[field(3, Apply::Rel32, 0, -4), field(10, Apply::Rel32, 1, -4), field(15, Apply::X86Branch32, 2, -4)],
    epilogue: &[0x58, 0xc3],
};

/// x86-64 Windows: `sub rsp, 40` (the callee's 32 bytes of home space, the stack aligned); each
/// call is `lea rcx, [rip + start]`, `lea rdx, [rip + end]`, `call init`; then `add rsp, 40`,
/// `ret`.
const WIN64_CALLS: Calls = Calls {
    prologue: &[0x48, 0x83, 0xec, 0x28],
    call: &[
        0x48, 0x8d, 0x0d, 0, 0, 0, 0, // lea rcx, [rip + start]
        0x48, 0x8d, 0x15, 0, 0, 0, 0, // lea rdx, [rip + end]
        0xe8, 0, 0, 0, 0, // call init
    ],
    fields: &[field(3, Apply::Rel32, 0, -4), field(10, Apply::Rel32, 1, -4), field(15, Apply::X86Branch32, 2, -4)],
    epilogue: &[0x48, 0x83, 0xc4, 0x28, 0xc3],
};

const A64_CALL: [u8; 20] = words_le([0x9000_0000, 0x9100_0000, 0x9000_0001, 0x9100_0021, 0x9400_0000]);
const A64_PROLOGUE: [u8; 8] = words_le([0xa9bf_7bfd, 0x9100_03fd]);
const A64_EPILOGUE: [u8; 8] = words_le([0xa8c1_7bfd, 0xd65f_03c0]);

/// AArch64: `stp x29, x30, [sp, #-16]!`, `mov x29, sp`; each call is `adrp x0, start`, `add x0,
/// x0, :lo12:start`, `adrp x1, end`, `add x1, x1, :lo12:end`, `bl init`; then `ldp x29, x30,
/// [sp], #16`, `ret`.
const A64_CALLS: Calls = Calls {
    prologue: &A64_PROLOGUE,
    call: &A64_CALL,
    fields: &[
        field(0, Apply::A64Page21, 0, 0),
        field(4, Apply::A64Lo12 { shift: 0 }, 0, 0),
        field(8, Apply::A64Page21, 1, 0),
        field(12, Apply::A64Lo12 { shift: 0 }, 1, 0),
        field(16, Apply::A64Call26, 2, 0),
    ],
    epilogue: &A64_EPILOGUE,
};

/// SanitizerCoverage's init constructor for `arch` in `format`'s calling convention.
pub(crate) fn calls(format: Format, arch: Arch) -> Result<&'static Calls, LinkError> {
    match (format, arch) {
        (Format::Pe, Arch::X86_64) => Ok(&WIN64_CALLS),
        (_, Arch::X86_64) => Ok(&X86_CALLS),
        (_, Arch::Aarch64) => Ok(&A64_CALLS),
        (_, Arch::Wasm32) => Err(error(None, "SanitizerCoverage on wasm: a module has no sections for its arrays")),
    }
}

/// The init function of each SanitizerCoverage section, by [`crate::stage::SANCOV_ELF`]'s
/// index: called in this order, counters, flags, guards, then the PC table.
pub(crate) const INIT: [&str; 4] = [
    "__sanitizer_cov_8bit_counters_init",
    "__sanitizer_cov_bool_flag_init",
    "__sanitizer_cov_trace_pc_guard_init",
    "__sanitizer_cov_pcs_init",
];

/// COFF's bounds of each section: `$A` sorts before the arrays' `$M` and `$Z` after, as
/// compiler-rt's `sanitizer_coverage_win_sections.cpp` brackets them; each an empty piece.
const COFF_BOUNDS: [(&str, &str); 4] =
    [(".SCOV$CA", ".SCOV$CZ"), (".SCOV$BA", ".SCOV$BZ"), (".SCOV$GA", ".SCOV$GZ"), (".SCOVP$A", ".SCOVP$Z")];

/// Each section's elements' alignment: where its first array starts, which its start bound
/// must be at.
const ALIGN: [u64; 4] = [1, 1, 4, 8];

/// The priorities LLVM gives the constructors: AddressSanitizer's first, then
/// SanitizerCoverage's.
pub(crate) const ASAN_PRIORITY: u32 = 1;
pub(crate) const SANCOV_PRIORITY: u32 = 2;

const OPTION: &str = "blaze v2 link msan option";
const CODE: &str = "blaze v2 link sanitizer constructor";
const POINTER: &str = "blaze v2 link sanitizer constructor pointer";
const GLOBALS: &str = "blaze v2 link asan globals";
const GLOBAL_NAME: &str = "blaze v2 link asan global name";
const ODR: &str = "blaze v2 link asan odr indicator";
const MODULE: &str = "blaze v2 link asan module name";
const BOUND: &str = "blaze v2 link sancov bound";

/// The calls of AddressSanitizer's constructor and destructor.
const REGISTER_GLOBALS: &str = "__asan_register_globals";
const UNREGISTER_GLOBALS: &str = "__asan_unregister_globals";

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The id of something synthesized: `part` in `domain`.
pub(crate) fn synthesized(domain: &'static str, part: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(domain);
    hasher.part(part);
    hasher.finish()
}

impl<'a> Stage<'a> {
    /// The key of a synthesized piece, interned.
    fn synthetic(&mut self, domain: &'static str, part: &[u8]) -> Sym {
        let id = self.symbols.id(&synthesized(domain, part));
        self.sym(SymKey::Data(id))
    }

    /// The same, when it was ever interned.
    fn find_synthetic(&self, domain: &'static str, part: &[u8]) -> Option<Sym> {
        let id = self.symbols.find_id(&synthesized(domain, part))?;
        self.symbols.find(SymKey::Data(id))
    }

    /// The class and rank the writer gives the core's pieces of `class`.
    fn core_class(&mut self, class: Class) -> (ClassId, u32) {
        let (class, rank) = self.writer.own(&class);
        (self.new.class_id(&class), rank)
    }

    fn unmake_sym(&mut self, sym: Option<Sym>) {
        if let Some((piece, _)) = sym.and_then(|it| self.new.home(it)) {
            self.unmake(piece);
        }
    }

    /// Before resolution, what the sanitizers add to the change: MemorySanitizer's options,
    /// the redzones after the change's globals and AddressSanitizer's constructors, and
    /// SanitizerCoverage's init constructor. Nothing without [`crate::Output::sanitize`].
    pub fn sanitize(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        let sanitize = self.output.sanitize;
        if sanitize.is_empty() {
            return Ok(());
        }
        match self.output.format {
            Format::Wasm => {
                return Err(error(None, "a sanitized wasm image: wasm has no sanitizer runtimes or sections for coverage"));
            }
            Format::Pe if !sanitize.sanitizers.is_empty() => {
                return Err(error(
                    None,
                    "a sanitized Windows image: rustc links MSVC's own runtime there (`/INFERASANLIBS`), \
                     which blaze's PE link does not take yet",
                ));
            }
            _ => {}
        }
        if sanitize.sanitizers.contains(Sanitizers::MEMORY) {
            self.msan_options();
        }
        if sanitize.sanitizers.contains(Sanitizers::ADDRESS) {
            self.redzones(change)?;
            self.asan_constructors()?;
        }
        if sanitize.coverage.is_on() {
            self.coverage_init()?;
        }
        Ok(())
    }

    /// `__msan_track_origins` (the build's `track_origins`) when it is not 0, and
    /// `__msan_keep_going` (1) when MemorySanitizer recovers: read-only `i32`s Rust names define,
    /// as LLVM defines them in each module for the runtime to read.
    fn msan_options(&mut self) {
        let sanitize = self.output.sanitize;
        let options = [
            ("__msan_track_origins", i32::from(sanitize.track_origins), sanitize.track_origins != 0),
            ("__msan_keep_going", 1, sanitize.recover.contains(Sanitizers::MEMORY)),
        ];
        for (name, value, wanted) in options {
            if !wanted {
                continue;
            }
            let sym = self.synthetic(OPTION, name.as_bytes());
            if self.new.home(sym).is_none() {
                let bytes: &'a [u8] = self.arena.alloc_slice_copy(&value.to_le_bytes());
                let (class, rank) = self.core_class(Class::Rodata);
                let piece = NewPiece { rank, ..piece_of(sym, class, 4, 4, &[]) };
                self.make(&piece, Content::Bytes(bytes));
            }
            let id = self.symbols.name(name);
            if self.new.names.get(&id) != Some(&sym) {
                Arc::make_mut(&mut self.new.names).insert(id, sym);
            }
            let named = self.sym(SymKey::Name(id));
            self.new.see(named);
        }
    }

    /// Every global of the change AddressSanitizer instruments, made again with its redzone
    /// after it and aligned to [`MIN_REDZONE`]: its statics, theirs and its functions' data.
    /// Only pieces this stage made: a datum already in the image has its redzone.
    fn redzones(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        let format = self.output.format;
        for it in &change.statics {
            if let Some(size) = instrumented_static(format, it) {
                let (item, instance) = (self.symbols.id(&it.item.0), self.symbols.id(&it.instance));
                let sym = self.sym(SymKey::Static(item, instance));
                self.redzone(sym, size);
            }
            for data in &it.data {
                self.data_redzone(data, None);
            }
        }
        for function in &change.added {
            let lsda = lsda(function);
            for data in &function.data {
                self.data_redzone(data, lsda);
            }
        }
        Ok(())
    }

    fn data_redzone(&mut self, data: &blaze_codegen::Data, lsda: Option<&ContentId>) {
        if let Some(size) = instrumented_data(data, lsda) {
            let id = self.symbols.id(&data.id);
            let sym = self.sym(SymKey::Data(id));
            self.redzone(sym, size);
        }
    }

    /// `sym`'s piece of `size` bytes, made this stage, made again `redzone(size)` bytes longer.
    fn redzone(&mut self, sym: Sym, size: u64) {
        let Some((id, 0)) = self.new.home(sym) else { return };
        if !self.fresh.contains(id) {
            return;
        }
        let piece = self.new.piece(id).expect("a fresh piece");
        if piece.size != size {
            return;
        }
        let arena = self.arena;
        let grown = NewPiece {
            sym,
            class: piece.class,
            rank: piece.rank(),
            order: piece.order(),
            align: MIN_REDZONE,
            size: size + redzone(size),
            zero: piece.zero(),
            fixups: arena.alloc_slice_copy(piece.fixups()),
            weak: arena.alloc_slice_copy(piece.weak()),
            defines: arena.alloc_slice_copy(piece.defines()),
            what: piece.what(),
        };
        let content = self.contents.get(id).unwrap_or(Content::Zero);
        self.unmake(id);
        self.make(&grown, content);
    }

    /// AddressSanitizer's constructor, calling `__asan_register_globals` with the descriptor
    /// array [`Stage::asan_globals`] makes, and its destructor calling
    /// `__asan_unregister_globals`, at [`ASAN_PRIORITY`]: on Mach-O the constructor registers
    /// the destructor with `__cxa_atexit` ([`at_exit`]). Made once.
    fn asan_constructors(&mut self) -> Result<(), LinkError> {
        let dtor = self.synthetic(CODE, UNREGISTER_GLOBALS.as_bytes());
        if self.new.home(dtor).is_some() {
            return Ok(());
        }
        let arch = self.arch();
        let descriptor = self.synthetic(GLOBALS, &[]);
        let unregister = self.sym_name(UNREGISTER_GLOBALS);
        self.code(dtor, register(arch)?, &[descriptor, unregister]);
        let ctor = self.synthetic(CODE, REGISTER_GLOBALS.as_bytes());
        let registers = self.sym_name(REGISTER_GLOBALS);
        if self.output.format == Format::MachO {
            let dso = self.sym_name("__dso_handle");
            let atexit = self.sym_name("__cxa_atexit");
            self.code(ctor, at_exit(arch)?, &[descriptor, registers, dtor, dso, atexit]);
        } else {
            self.code(ctor, register(arch)?, &[descriptor, registers]);
            self.constructor(UNREGISTER_GLOBALS.as_bytes(), dtor, ASAN_PRIORITY, true)?;
        }
        self.constructor(REGISTER_GLOBALS.as_bytes(), ctor, ASAN_PRIORITY, false)?;
        Ok(())
    }

    /// A piece of `stub`'s code, its fields of `targets`, the names among them seen.
    fn code(&mut self, sym: Sym, stub: Stub, targets: &[Sym]) {
        let fixups = collect_in(stub.fields.iter().map(|it| fixup(it, targets, 0)), self.arena).leak();
        let (class, rank) = self.core_class(Class::Text);
        let piece = NewPiece { rank, ..piece_of(sym, class, 16, stub.code.len() as u64, fixups) };
        self.make(&piece, Content::Bytes(stub.code));
        for target in targets {
            if matches!(self.symbols.key(*target), SymKey::Name(_)) {
                self.new.see(*target);
            }
        }
    }

    /// The pointer to `code` its format's loader calls before `main` (or at exit, `fini`) at
    /// LLVM's `priority`: ELF's `.init_array.N` and `.fini_array.N`, Mach-O's
    /// `__mod_init_func` below every input's (and above a dev run's start hook), COFF's
    /// `.CRT$XC` as LLVM names a priority below 200.
    fn constructor(&mut self, key: &[u8], code: Sym, priority: u32, fini: bool) -> Result<Sym, LinkError> {
        let sym = self.synthetic(POINTER, key);
        if self.new.home(sym).is_some() {
            return Ok(sym);
        }
        let mut name = [0u8; 64];
        let section = match (self.output.format, fini) {
            (Format::MachO, false) => None,
            (Format::Elf, false) => Some(".init_array."),
            (Format::Elf, true) => Some(".fini_array."),
            (Format::Pe, false) => Some(".CRT$XCC"),
            (Format::MachO | Format::Pe, true) => {
                return Err(error(None, "a destructor array on Mach-O or COFF, whose loaders call none at exit"));
            }
            (Format::Wasm, _) => return Err(error(None, "a constructor in a wasm module")),
        };
        let (class, rank) = match section {
            Some(prefix) => {
                let mut digits = [0u8; 5];
                let mut left = priority;
                for digit in digits.iter_mut().rev() {
                    *digit = b'0' + (left % 10) as u8;
                    left /= 10;
                }
                let digits = std::str::from_utf8(&digits).expect("digits");
                self.writer.section(joined(&mut name, &[prefix, digits]), false, false, false).map_err(|it| error(None, it))?
            }
            None => (Class::InitArray, priority),
        };
        let class = self.new.class_id(&class);
        let fixups = collect_in([Fixup { at: 0, apply: Apply::Abs64, target: code, addend: 0, minus: None }], self.arena).leak();
        let piece = NewPiece { rank, ..piece_of(sym, class, 8, 8, fixups) };
        self.make(&piece, Content::Zero);
        Ok(sym)
    }

    /// The class of SanitizerCoverage's section `kind` ([`crate::stage::SANCOV_ELF`]), when the
    /// image has one.
    fn sancov_class_of(&self, kind: usize) -> Option<Class> {
        let name = match self.output.format {
            Format::Elf => SANCOV_ELF[kind],
            Format::MachO => SANCOV_MACHO[kind],
            Format::Pe => SANCOV_COFF[kind],
            Format::Wasm => return None,
        };
        self.writer.section(name, true, false, false).ok().map(|it| it.0)
    }

    /// Which SanitizerCoverage sections the image has a piece of: by its own pieces in a full
    /// link; in an incremental one, those its init constructor called and those this stage
    /// made a piece of. A section whose last function an incremental link removes is still
    /// called for, with an empty range, until the next full link: libFuzzer takes it as
    /// nothing, the fuzz engine as a module of no counters.
    fn coverage_present(&self, ctor: Option<PieceId>) -> [bool; 4] {
        let mut classes: [Option<ClassId>; 4] = [None; 4];
        for (kind, class) in classes.iter_mut().enumerate() {
            *class = self.sancov_class_of(kind).and_then(|it| self.new.find_class(&it));
        }
        let mut present = [false; 4];
        let mut see = |id: PieceId| {
            let Some(piece) = self.new.piece(id) else { return };
            if !classes.contains(&Some(piece.class)) {
                return;
            }
            match self.symbols.key(piece.sym) {
                SymKey::Coverage(_, kind) if (kind as usize) < 3 => present[kind as usize] = true,
                SymKey::CoveragePcs(_) => present[3] = true,
                _ => {}
            }
        };
        if self.full {
            for piece in self.new.live_pieces() {
                see(piece.id);
            }
            return present;
        }
        for id in self.fresh.iter() {
            see(id);
        }
        let called = self.called(ctor);
        for (present, called) in present.iter_mut().zip(called) {
            *present |= called;
        }
        present
    }

    /// The sections SanitizerCoverage's constructor `ctor` calls the init function of.
    fn called(&self, ctor: Option<PieceId>) -> [bool; 4] {
        let mut out = [false; 4];
        if let Some(piece) = ctor.and_then(|it| self.new.piece(it)) {
            for fixup in piece.fixups() {
                if let Some(kind) = INIT.iter().position(|it| self.symbols.name_of(fixup.target) == Some(*it)) {
                    out[kind] = true;
                }
            }
        }
        out
    }

    /// SanitizerCoverage's constructor, at [`SANCOV_PRIORITY`], calling each section's init
    /// function with its bounds: ELF's `__start_`/`__stop_` symbols, Mach-O's
    /// `section$start$`/`section$end$`, COFF's empty pieces in the sections sorted around it.
    /// Made again when the sections the image has change.
    fn coverage_init(&mut self) -> Result<(), LinkError> {
        let code = self.synthetic(CODE, b"sancov");
        let old = self.new.home(code).map(|it| it.0);
        let present = self.coverage_present(old);
        let kinds = present.iter().filter(|it| **it).count();
        if let Some(old) = old {
            if self.called(Some(old)) == present {
                return Ok(());
            }
            self.unmake(old);
        }
        if kinds == 0 {
            let pointer = self.find_synthetic(POINTER, b"sancov");
            self.unmake_sym(pointer);
            return Ok(());
        }
        let calls = calls(self.output.format, self.arch())?;
        let len = calls.prologue.len() + kinds * calls.call.len() + calls.epilogue.len();
        let bytes: &'a mut [u8] = self.arena.alloc_slice_fill_with(len, |_| 0);
        bytes[..calls.prologue.len()].copy_from_slice(calls.prologue);
        bytes[len - calls.epilogue.len()..].copy_from_slice(calls.epilogue);
        let mut fixups: FrameVec<'a, Fixup> = Vec::with_capacity_in(kinds * calls.fields.len(), self.arena);
        let mut at = calls.prologue.len();
        for kind in (0..4).filter(|it| present[*it]) {
            bytes[at..at + calls.call.len()].copy_from_slice(calls.call);
            let (start, end) = self.coverage_bounds_of(kind)?;
            let init = self.sym_name(INIT[kind]);
            self.new.see(init);
            let targets = [start, end, init];
            fixups.extend(calls.fields.iter().map(|it| fixup(it, &targets, at as u64)));
            at += calls.call.len();
        }
        let (class, rank) = self.core_class(Class::Text);
        let piece = NewPiece { rank, ..piece_of(code, class, 16, len as u64, fixups.leak()) };
        self.make(&piece, Content::Bytes(bytes));
        self.constructor(b"sancov", code, SANCOV_PRIORITY, false)?;
        Ok(())
    }

    /// The symbols of the start and end of SanitizerCoverage's section `kind`.
    fn coverage_bounds_of(&mut self, kind: usize) -> Result<(Sym, Sym), LinkError> {
        let mut name = [0u8; 64];
        match self.output.format {
            Format::Elf => {
                let section = SANCOV_ELF[kind];
                let start = self.sym_name(joined(&mut name, &["__start_", section]));
                let end = self.sym_name(joined(&mut name, &["__stop_", section]));
                Ok((start, end))
            }
            Format::MachO => {
                let (segment, section) = SANCOV_MACHO[kind].split_once(',').expect("SEGMENT,section");
                let start = self.sym_name(joined(&mut name, &["section$start$", segment, "$", section]));
                let end = self.sym_name(joined(&mut name, &["section$end$", segment, "$", section]));
                Ok((start, end))
            }
            Format::Pe => {
                let (start, end) = COFF_BOUNDS[kind];
                Ok((self.coff_bound(start, ALIGN[kind])?, self.coff_bound(end, 1)?))
            }
            Format::Wasm => Err(error(None, "SanitizerCoverage on wasm: a module has no sections for its arrays")),
        }
    }

    /// An empty piece in the COFF section `name`, made once: its address is a bound of the
    /// arrays sorted after or before it.
    fn coff_bound(&mut self, name: &str, align: u64) -> Result<Sym, LinkError> {
        let sym = self.synthetic(BOUND, name.as_bytes());
        if self.new.home(sym).is_none() {
            let (class, rank) = self.writer.section(name, true, false, false).map_err(|it| error(None, it))?;
            let class = self.new.class_id(&class);
            let piece = NewPiece { rank, ..piece_of(sym, class, align, 0, &[]) };
            self.make(&piece, Content::Zero);
        }
        Ok(sym)
    }

    /// After the sweep, AddressSanitizer's descriptor array: a count, then an `__asan_global`
    /// of each instrumented global still in the image, in the order of their keys, made again
    /// when one is added or gone. Its name, the image's name and an ODR indicator byte of each
    /// are pieces of their own.
    pub fn asan_globals(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        let elf_or_macho = matches!(self.output.format, Format::Elf | Format::MachO);
        if !self.output.sanitize.sanitizers.contains(Sanitizers::ADDRESS) || !elf_or_macho {
            return Ok(());
        }
        let arena = self.arena;
        let descriptor = self.synthetic(GLOBALS, &[]);
        let old = self.new.home(descriptor).map(|it| it.0);
        let mut entries: FrameVec<'a, (Sym, u64)> = Vec::new_in(arena);
        // The old entries' globals that are gone or made again this stage.
        let mut gone: FrameVec<'a, Sym> = Vec::new_in(arena);
        if let Some(old) = old {
            let piece = self.new.piece(old).expect("the descriptor");
            let bytes = piece.bytes_in(arena);
            for fixup in piece.fixups() {
                if fixup.at < HEADER || (fixup.at - HEADER) % GLOBAL != BEG {
                    continue;
                }
                let at = (fixup.at + SIZE) as usize;
                let size = bytes
                    .get(at..at + 8)
                    .map(|it| u64::from_le_bytes(it.try_into().expect("8 bytes")))
                    .ok_or_else(|| error(None, "AddressSanitizer's descriptor array is not in its region"))?;
                match self.new.home(fixup.target) {
                    Some((id, 0)) if !self.fresh.contains(id) => entries.push((fixup.target, size)),
                    _ => gone.push(fixup.target),
                }
            }
        }
        let kept = entries.len();
        let format = self.output.format;
        for it in &change.statics {
            if let Some(size) = instrumented_static(format, it) {
                let (Some(item), Some(instance)) = (self.symbols.find_id(&it.item.0), self.symbols.find_id(&it.instance)) else {
                    continue;
                };
                if let Some(sym) = self.symbols.find(SymKey::Static(item, instance)) {
                    self.fresh_entry(sym, size, &mut entries);
                }
            }
            for data in &it.data {
                if let Some(size) = instrumented_data(data, None) {
                    self.fresh_data_entry(data, size, &mut entries);
                }
            }
        }
        for function in &change.added {
            let lsda = lsda(function);
            for data in &function.data {
                if let Some(size) = instrumented_data(data, lsda) {
                    self.fresh_data_entry(data, size, &mut entries);
                }
            }
        }
        if old.is_some() && gone.is_empty() && entries.len() == kept {
            return Ok(());
        }
        let symbols = &*self.symbols;
        entries.sort_unstable_by(|a, b| symbols.cmp(a.0, b.0));
        entries.dedup_by_key(|it| it.0);
        for sym in gone {
            if entries.binary_search_by(|it| self.symbols.cmp(it.0, sym)).is_ok() {
                continue;
            }
            let name = self.entry_key(sym);
            let found = self.find_synthetic(GLOBAL_NAME, name);
            self.unmake_sym(found);
            let found = self.find_synthetic(ODR, name);
            self.unmake_sym(found);
        }
        if let Some(old) = old {
            self.unmake(old);
        }

        let module = self.module_name();
        let size = HEADER + GLOBAL * entries.len() as u64;
        let bytes: &'a mut [u8] = arena.alloc_slice_fill_with(size as usize, |_| 0);
        bytes[..8].copy_from_slice(&(entries.len() as u64).to_le_bytes());
        let mut fixups: FrameVec<'a, Fixup> = Vec::with_capacity_in(4 * entries.len(), arena);
        for (n, &(sym, size)) in entries.iter().enumerate() {
            let at = HEADER + GLOBAL * n as u64;
            let put = |bytes: &mut [u8], field: u64, value: u64| {
                let field = (at + field) as usize;
                bytes[field..field + 8].copy_from_slice(&value.to_le_bytes());
            };
            put(bytes, SIZE, size);
            put(bytes, SIZE_WITH_REDZONE, size + redzone(size));
            let (name, odr) = self.entry_pieces(sym)?;
            for (field, target) in [(BEG, sym), (NAME, name), (MODULE_NAME, module), (ODR_INDICATOR, odr)] {
                fixups.push(Fixup { at: at + field, apply: Apply::Abs64, target, addend: 0, minus: None });
            }
        }
        let (class, rank) = self.core_class(Class::Data);
        let piece = NewPiece { rank, ..piece_of(descriptor, class, 8, size, fixups.leak()) };
        self.make(&piece, Content::Bytes(bytes));
        Ok(())
    }

    /// `sym` of `size` bytes, when this stage made its piece and gave it its redzone.
    fn fresh_entry(&self, sym: Sym, size: u64, entries: &mut FrameVec<'a, (Sym, u64)>) {
        let Some((id, 0)) = self.new.home(sym) else { return };
        let grown = self.new.piece(id).is_some_and(|it| it.size == size + redzone(size));
        if grown && self.fresh.contains(id) {
            entries.push((sym, size));
        }
    }

    fn fresh_data_entry(&self, data: &blaze_codegen::Data, size: u64, entries: &mut FrameVec<'a, (Sym, u64)>) {
        let Some(id) = self.symbols.find_id(&data.id) else { return };
        if let Some(sym) = self.symbols.find(SymKey::Data(id)) {
            self.fresh_entry(sym, size, entries);
        }
    }

    /// The bytes an entry's own pieces are keyed by: its global's key.
    fn entry_key(&mut self, sym: Sym) -> &'a [u8] {
        self.scratch.clear();
        self.symbols.write_key(sym, &mut *self.scratch);
        self.arena.alloc_slice_copy(self.scratch.as_slice())
    }

    /// An entry's name, NUL-terminated (`static ` or `data ` and the first 8 bytes of its id
    /// in hex: what reports call the global), and its ODR indicator byte, made once.
    fn entry_pieces(&mut self, sym: Sym) -> Result<(Sym, Sym), LinkError> {
        let key = self.entry_key(sym);
        let name = self.synthetic(GLOBAL_NAME, key);
        if self.new.home(name).is_none() {
            let (prefix, id): (&[u8], _) = match self.symbols.key(sym) {
                SymKey::Static(item, _) => (b"static ", item),
                SymKey::Data(id) => (b"data ", id),
                // alloc: error path
                _ => return Err(error(None, format!("an AddressSanitizer global of {}", self.symbols.describe(sym)))),
            };
            let id = self.symbols.content(id).0;
            let bytes: &'a mut [u8] = self.arena.alloc_slice_fill_with(prefix.len() + 17, |_| 0);
            bytes[..prefix.len()].copy_from_slice(prefix);
            for (at, byte) in id[..8].iter().enumerate() {
                bytes[prefix.len() + 2 * at] = HEX[usize::from(byte >> 4)];
                bytes[prefix.len() + 2 * at + 1] = HEX[usize::from(byte & 15)];
            }
            let (class, rank) = self.core_class(Class::Rodata);
            let piece = NewPiece { rank, ..piece_of(name, class, 1, bytes.len() as u64, &[]) };
            self.make(&piece, Content::Bytes(bytes));
        }
        let odr = self.synthetic(ODR, key);
        if self.new.home(odr).is_none() {
            let (class, rank) = self.core_class(Class::Data);
            let piece = NewPiece { rank, ..piece_of(odr, class, 1, 1, &[]) };
            self.make(&piece, Content::Zero);
        }
        Ok((name, odr))
    }

    /// The image's file name, NUL-terminated: every entry's `module_name`.
    fn module_name(&mut self) -> Sym {
        let sym = self.synthetic(MODULE, &[]);
        if self.new.home(sym).is_none() {
            let name = self.name.as_bytes();
            let bytes: &'a mut [u8] = self.arena.alloc_slice_fill_with(name.len() + 1, |_| 0);
            bytes[..name.len()].copy_from_slice(name);
            let (class, rank) = self.core_class(Class::Rodata);
            let piece = NewPiece { rank, ..piece_of(sym, class, 1, bytes.len() as u64, &[]) };
            self.make(&piece, Content::Bytes(bytes));
        }
        sym
    }

    /// After placement, SanitizerCoverage's constructor relocated again when a section it
    /// gives the bounds of by the linker's symbols moved or grew: an incremental link relays
    /// a section in place, and nothing else refers to those symbols' values changing.
    pub fn coverage_moved(&mut self) {
        if !self.output.sanitize.coverage.is_on() || self.full || !matches!(self.output.format, Format::Elf | Format::MachO) {
            return;
        }
        let Some((ctor, _)) = self.find_synthetic(CODE, b"sancov").and_then(|it| self.new.home(it)) else { return };
        if self.write.contains(ctor) {
            return;
        }
        for kind in 0..4 {
            let Some(class) = self.sancov_class_of(kind) else { continue };
            let region = |state: &crate::state::State| {
                state.space_of(&class).map(|at| (state.spaces[at].region.addr, state.spaces[at].region.size))
            };
            if region(&self.new) != region(self.old) {
                self.write.insert(ctor);
                return;
            }
        }
    }
}

/// A synthesized piece's fields: rank 0, the order of the core's pieces, not zeroed.
fn piece_of<'x>(sym: Sym, class: ClassId, align: u64, size: u64, fixups: &'x [Fixup]) -> NewPiece<'x> {
    NewPiece {
        sym,
        class,
        rank: 0,
        order: [u32::MAX; 3],
        align: align.max(1),
        size,
        zero: false,
        fixups,
        weak: &[],
        defines: &[],
        what: What::Fixed,
    }
}

/// `parts` one after another in `out`, which has room for them: a linker symbol's name.
fn joined<'o>(out: &'o mut [u8; 64], parts: &[&str]) -> &'o str {
    let mut len = 0;
    for part in parts {
        out[len..len + part.len()].copy_from_slice(part.as_bytes());
        len += part.len();
    }
    std::str::from_utf8(&out[..len]).expect("joined from strs")
}

/// `field` of code at `base` in its piece, its target one of `targets`.
fn fixup(field: &Field, targets: &[Sym], base: u64) -> Fixup {
    Fixup {
        at: base + field.at,
        apply: field.apply,
        target: targets[usize::from(field.target)],
        addend: field.addend,
        minus: None,
    }
}
