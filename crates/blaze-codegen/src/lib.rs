//! Per-function codegen (docs/v2/plan/lanes/codegen.md section 9, docs/v2/plan/lanes/items.md section 6.4).
//!
//! The unit of codegen is a function [`Instance`]: a generic's code hash and the facts of its
//! type arguments it reads, so identical instances across crates, branches and projects
//! compile once, and identical normalized MIR anywhere is one compiled function. Each compiles
//! to a [`CompiledFunction`]: its machine code with its relocations, keyed by its
//! [`InstanceKey`], with debug info and panic locations in a side table under the same key.
//!
//! Dev: Cranelift per function. Release: LLVM per function, with the callees it inlined
//! recorded ([`CompiledFunction::inlined`]) as deep-hash edges.
//!
//! Code dedupe never merges statics or anything whose address the program can compare as
//! distinct: those are [`Target::Static`], keyed by which item they are, never by content.

#![feature(allocator_api, allocator_ext)]
// Unsafe code is in `memo` alone (docs/v2/plan/design/unsafe.md).
#![deny(unsafe_code)]

use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use serde::{Deserialize, Serialize};

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub mod backend;
pub mod blob;
pub mod content;
pub mod dedupe;
pub mod driver;
pub mod exec;
pub mod flight;
pub mod fold;
pub mod imports;
#[allow(unsafe_code)]
pub mod memo;
pub mod plan;
pub mod query;
pub mod relocs;
pub mod settings;
pub mod summary;
pub mod template;
pub mod wasm;
pub mod unwind;

pub use driver::Owner;
pub use exec::ExecTable;
pub use relocs::{Reloc, Relocation, Relocs, RelocsView};

/// A function instance: what is compiled.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Instance {
    /// The generic's (or the plain function's) code hash.
    pub code: CodeHash,
    /// The content ID of the facts of its type arguments its code reads (layouts, impls
    /// selected, constants): not the arguments' names.
    pub args: ContentId,
    /// The codegen settings that change its machine code.
    pub settings: Settings,
}

/// An instance's key: its content ID, the store's key of its blob.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct InstanceKey(pub ContentId);

pub const INSTANCE_DOMAIN: &str = "blaze v2 instance 1";

impl Instance {
    pub fn key(&self) -> InstanceKey {
        let mut hasher = Hasher::new(INSTANCE_DOMAIN);
        hasher.id(&self.code.0).id(&self.args).id(&self.settings.0);
        InstanceKey(hasher.finish())
    }
}

/// The content ID of everything that changes machine code besides the function: target,
/// backend and its version, opt-level, overflow checks, panic strategy, debug-info level,
/// the toolchain. Built by the codegen lane; any setting left out is a stale-code bug.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Settings(pub ContentId);

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Backend {
    /// `speed`: Cranelift's `opt_level=speed`, for code no edit reaches (std's, dependencies',
    /// build-time code), which is compiled once; else `opt_level=none`.
    Cranelift { speed: bool },
    Llvm { opt: u8 },
    /// Copy-and-patch (`blaze-stencil`): tier 0 of a tiered dev or test run
    /// (docs/v2/plan/lanes/fastcompile/stencil.md).
    Stencil,
}

impl Backend {
    pub const fn is_cranelift(self) -> bool {
        matches!(self, Backend::Cranelift { .. })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Arch {
    X86_64,
    Aarch64,
    Wasm32,
}

/// One unit of codegen: an instance and how to compile it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Unit {
    pub instance: Instance,
    pub backend: Backend,
    pub arch: Arch,
    /// The item it is an instance of, for its name where the program observes it (symbol,
    /// `#[no_mangle]`) and for diagnostics; never part of [`Instance::key`].
    pub item: ItemKey,
}

/// A function's machine code, relocatable: placed anywhere by the linker, which applies
/// `relocs`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: in `driver::Built` and `TieredUp`; a build moves its functions, tests copy one to vary it
pub struct CompiledFunction {
    pub key: InstanceKey,
    pub arch: Arch,
    /// A power of two.
    pub align: u32,
    pub code: Vec<u8>,
    /// Where `code` holds addresses: the places the linker fills, each target once.
    pub relocs: Relocs,
    /// Read-only data only it refers to, deduplicated by content: [`Target::Data`].
    pub data: Vec<Data>,
    /// The callees whose bodies it inlined: a change to one's code reruns it (release).
    pub inlined: Vec<CodeHash>,
    /// Where its debug info and panic locations are, in the side table.
    pub side: Option<ContentId>,
    /// How to unwind through it: none for a function that can't unwind (`nounwind`, wasm).
    pub unwind: Option<Unwind>,
    /// Its code offsets and the spans they are of, by `at`, for debug info. Spans are
    /// position-independent (anchor and index), so a reformat changes no function.
    pub lines: Vec<Line>,
    /// What SanitizerCoverage made for it ([`crate::settings::Coverage`]): `None` without.
    pub coverage: Option<Covered>,
    /// What PGO made of it ([`crate::settings::Pgo`]): `None` built without.
    pub profile: Option<Profiled>,
}

/// What PGO made of one function.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Profiled {
    /// Instrumented (`Pgo::Generate`): its record, from which the link writes the program's
    /// `__llvm_prf_data`, `__llvm_prf_names` and `__llvm_prf_cnts`. Its code counts into its
    /// own counters ([`Target::ProfileCounters`]).
    Counted(ProfileRecord),
    /// Built with a profile (`Pgo::Use`). `mismatched`: the profile has a record under its
    /// name with another control flow hash (it was edited since), so none of its counts
    /// applied; the build's summary counts these ([`crate::driver::Stats::profile_mismatched`]).
    Used { mismatched: bool },
}

impl Profiled {
    pub fn counted(&self) -> Option<&ProfileRecord> {
        match self {
            Profiled::Counted(record) => Some(record),
            Profiled::Used { .. } => None,
        }
    }

    pub fn mismatched(&self) -> bool {
        matches!(self, Profiled::Used { mismatched: true })
    }
}

/// LLVM's PGO record of an instrumented function (`__profd_`), as LLVM's raw profile has it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProfileRecord {
    /// The name its counts are kept under, as its bytes: [`profile_name`] of them.
    pub name: [u8; 16],
    /// The record's `NameRef`, the low 64 bits of the name's MD5.
    pub name_ref: u64,
    /// The record's `FuncHash`, its control flow's hash.
    pub hash: u64,
    /// Its 8-byte counters, which start zeroed: the record's `NumCounters`.
    pub counters: u32,
    /// `__llvm_profile_raw_version`: one value for a build.
    pub version: u64,
}

impl ProfileRecord {
    /// The name its counts are kept under, for `__llvm_prf_names`.
    pub fn name(&self) -> String {
        profile_name(&self.name)
    }
}

/// The name a function's PGO counts are kept under: `blaze.pgo.` and `name` in lowercase hex.
pub fn profile_name(name: &[u8; 16]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    // alloc: once per function built with PGO, or an instrumented one the link names
    let mut out = String::with_capacity(PROFILE_NAME_PREFIX.len() + 32);
    out.push_str(PROFILE_NAME_PREFIX);
    for byte in name {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 15)] as char);
    }
    out
}

pub const PROFILE_NAME_PREFIX: &str = "blaze.pgo.";

/// What LLVM's SanitizerCoverage made for one function: its arrays, which start zeroed and
/// which the link gathers with every other function's into one section each, in one order,
/// and its PC table.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)] // clone: a template's view hands it to the function it makes, once per instantiate with coverage
pub struct Covered {
    /// Bytes of its 8-bit counters (`__sancov_cntrs`), flags (`__sancov_bools`) and
    /// guards (`__sancov_guards`, 4 bytes each); 0 for an array it has not.
    pub counters: u32,
    pub flags: u32,
    pub guards: u32,
    /// Of each block it counts, in the order of its arrays: how far into `code` it starts,
    /// and its flags (1 for the function's entry), as `__sancov_pcs` has them.
    pub pcs: Vec<(u32, u64)>,
}

impl Covered {
    /// How many arrays a function has: [`Target::Coverage`] names one by its index.
    pub const ARRAYS: u32 = 3;
}

/// A code offset of a function and the span its instructions from there are of.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Line {
    pub at: u32,
    pub span: SpanRef,
}

/// A function's unwind information, as its backend made it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: inside `CompiledFunction`
pub enum Unwind {
    /// DWARF call frame information (ELF, Mach-O): the CIE's fields and initial instructions,
    /// and the FDE's instructions, as `.eh_frame` encodes them (DW_CFA_*).
    Dwarf {
        code_align: u32,
        data_align: i32,
        return_address: u16,
        cie: Vec<u8>,
        fde: Vec<u8>,
        /// The personality routine (`rust_eh_personality`) of a function with landing pads.
        personality: Option<Target>,
        /// Its language-specific data area: one of the function's `data`, by id.
        lsda: Option<ContentId>,
    },
    /// Windows: x64's `UNWIND_INFO` or ARM64's `.xdata` record (header, then unwind codes),
    /// without the handler; and the handler and its data.
    Windows { info: Vec<u8>, handler: Option<Target>, lsda: Option<ContentId> },
}

/// A static's definition. Its address is the linker's: by which static it is, never by
/// content ([`Target::Static`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: in `driver::Built`; a build moves its statics, tests copy one to vary it
pub struct Static {
    pub item: ItemKey,
    pub instance: ContentId,
    /// A power of two.
    pub align: u32,
    pub init: Init,
    /// Read-only data only its value refers to ([`Target::Data`]): the bytes of a `&str`,
    /// a promoted constant.
    pub data: Vec<Data>,
    /// Written while the program runs: `static mut`, or a type with interior mutability.
    pub mutable: bool,
    pub thread_local: bool,
    /// `#[link_section]`, as written.
    pub section: Option<String>,
    /// On wasm, what each `Target::Extern` its relocations name is imported as when nothing
    /// defines it (a pointer to a `#[link(wasm_import_module)]` function), by the extern's
    /// name; empty elsewhere.
    pub imports: Vec<(String, wasm::Import)>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: inside `Static`
pub enum Init {
    Bytes { bytes: Vec<u8>, relocs: Relocs },
    Zeroed { size: u64 },
}

/// Read-only bytes a function refers to: string literals, constants, vtables of known types.
/// Or, `writable`, a static's nested mutable allocation ([`Data::owned`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: inside `CompiledFunction` and `Static`
pub struct Data {
    pub id: ContentId,
    pub align: u32,
    pub bytes: Vec<u8>,
    pub relocs: Relocs,
    /// Written while the program runs: memory a static's value points to that is mutable,
    /// as rustc interns it (behind a `&mut` in a `static mut`, or interior mutability behind a
    /// `&`). rustc emits it as a global of its own, writable and never merged with another;
    /// so is this, its ID its static's and not its content's alone.
    pub writable: bool,
}

/// How the address is written, as the object formats name them.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum RelocKind {
    /// The absolute address, 64 bits.
    Abs64,
    /// x86-64: 32 bits relative to the end of the field (`R_X86_64_PC32`, `PLT32`).
    PcRel32,
    /// x86-64: the GOT entry's address, relative (`R_X86_64_GOTPCREL`).
    GotPcRel32,
    /// AArch64 `BL`/`B`: 26 bits of words (`R_AARCH64_CALL26`).
    Call26,
    /// AArch64 `ADRP`: the page, 21 bits (`R_AARCH64_ADR_PREL_PG_HI21`).
    AdrpPage21,
    /// AArch64 `ADD`/`LDR`/`STR`: the low 12 bits, scaled as the instruction's access size
    /// says (`R_AARCH64_ADD_ABS_LO12_NC`, `LDST{8,16,32,64,128}_ABS_LO12_NC`); the linker
    /// reads the scale from the instruction.
    PageOffset12,
    /// AArch64 `LDR` from the GOT (`R_AARCH64_ADR_GOT_PAGE`, `LD64_GOT_LO12_NC`).
    GotPage21,
    GotPageOffset12,
    /// Thread-local, local exec: the offset in the thread's block (x86-64 ELF `TPOFF32`).
    TlsOffset,
    /// wasm: a function or table index, a memory address.
    WasmFunction,
    WasmTable,
    WasmMemory,
    /// x86-64 ELF general dynamic: the GOT pair `__tls_get_addr` takes (`R_X86_64_TLSGD`).
    TlsGd32,
    /// x86-64 ELF initial exec: the GOT cell holding the offset (`R_X86_64_GOTTPOFF`).
    TlsGotTpOff32,
    /// x86-64 Mach-O: `rel32` to the cell holding the variable's descriptor
    /// (`X86_64_RELOC_TLV`).
    TlvRel32,
    /// AArch64 Mach-O: that cell's page and low 12 bits for an `LDR`
    /// (`ARM64_RELOC_TLVP_LOAD_PAGE21`, `TLVP_LOAD_PAGEOFF12`).
    TlvPage21,
    TlvPageOffset12,
    /// AArch64 ELF TLS descriptor: the GOT pair's page, its `LDR`, its `ADD`, and the call
    /// (`R_AARCH64_TLSDESC_ADR_PAGE21`, `LD64_LO12`, `ADD_LO12`, `CALL`).
    TlsDescPage21,
    TlsDescLd64Lo12,
    TlsDescAddLo12,
    TlsDescCall,
    /// AArch64 ELF initial exec: the GOT cell's page and low 12 bits
    /// (`R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21`, `LD64_GOTTPREL_LO12_NC`).
    TlsIePage21,
    TlsIeLo12,
    /// AArch64 ELF local exec: the offset's high and low 12 bits
    /// (`R_AARCH64_TLSLE_ADD_TPREL_HI12`, `ADD_TPREL_LO12_NC`).
    TpHi12,
    TpLo12,
    /// COFF: the offset in its section (`IMAGE_REL_AMD64_SECREL`, `IMAGE_REL_ARM64_SECREL`):
    /// a thread-local's place in the `.tls` block.
    SectionRel32,
}

/// What a relocation points at.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)] // clone: a function owns its targets, so an instantiate copies each place's; only an outside symbol's name allocates
pub enum Target {
    /// Another compiled function, called through its jump-table slot so that a patch of it
    /// changes one slot.
    Function(InstanceKey),
    /// The same, called directly: release code, or a callee known not to be patched.
    Direct(InstanceKey),
    /// A static: by which item and instance it is, never merged by content.
    Static { item: ItemKey, instance: ContentId },
    /// Read-only data, merged by content.
    Data(ContentId),
    /// A symbol outside blaze's code: libc, a C dependency, the system.
    Extern(String),
    /// As [`Target::Extern`], linked weakly: 0 when nothing defines it
    /// (`#[linkage = "extern_weak"]`).
    Weak(String),
    /// A position the program observes (`line!()`, `#[track_caller]`'s `Location`): the
    /// span side table's entry, filled at link time, so a reformat compiles nothing.
    Position(SpanRef),
    /// The function's own SanitizerCoverage array ([`CompiledFunction::coverage`]): 0 its
    /// counters, 1 its flags, 2 its guards; the relocation's addend is into it. A function
    /// names no other's, and nothing else names one.
    Coverage(u32),
    /// The function's own two tier-up counter cells (`blaze_stencil::tier`: 16 bytes,
    /// 8-aligned, zeroed): its calls at addend 0, its back edges at 8. Written by its own
    /// code, read by the daemon, placed writable by the linker one pair per instance and
    /// never merged. The key is the function's own: a function names no other's cells, and
    /// nothing but a function names any.
    Counter(InstanceKey),
    /// The function's own PGO counters ([`ProfileRecord::counters`] of its
    /// [`Profiled::Counted`], 8 bytes each, zeroed): one array a function, so no index; the
    /// relocation's addend is into it. A function names no other's, and nothing else names one.
    ProfileCounters,
}

/// A native library a crate links against: a `#[link(..)]` of one of its `extern` blocks, a
/// build script's `cargo:rustc-link-lib` or a `-l`, as rustc's `NativeLib` has it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)] // clone: setup, once per crate's link list
pub struct NativeLib {
    /// As written: `foo` for `libfoo.a`, `foo.dll` or `foo.framework`, unless `verbatim`.
    pub name: String,
    pub kind: NativeLibKind,
    /// `+verbatim`: `name` is the file's, with no `lib` or extension added.
    pub verbatim: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NativeLibKind {
    /// `dylib`, and no `kind`: the linker takes whichever it finds, as rustc passes `-l`.
    Dylib,
    /// `whole`: `+whole-archive`, every object linked, reached or not. `bundle`: not
    /// `-bundle`, its objects go into the archive of an `rlib` or `staticlib` that links it.
    Static { whole: bool, bundle: bool },
    /// An Apple framework.
    Framework,
}

/// Why a unit did not compile: names the instance and the item.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct CodegenError {
    pub unit: InstanceKey,
    pub item: ItemKey,
    pub what: String,
}

/// A backend: compiles one unit, given the MIR the front end built for it.
pub trait Compile: Sync {
    /// The front end's MIR of an instance, as the codegen lane reads it (`hir-ty-mir`).
    type Mir<'a>;
    fn compile(
        &self,
        unit: &Unit,
        mir: Self::Mir<'_>,
    ) -> Result<CompiledFunction, CodegenError>;
}
