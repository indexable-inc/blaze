//! DWARF for the functions of an object: which line every instruction is of (`.debug_line`)
//! and what every function is called and where it is declared (`.debug_info`). A backtrace
//! and a debugger read the same tables rustc writes for its own code, so a frame of ours
//! reads like one of rustc's.
//!
//! A compiled function knows its statements by their place in the body ([`LineRow`]), which
//! an edit above the function leaves alone. The lines are looked up when the function is
//! stitched into an object ([`FunctionDebug`]), and all functions of the object share one
//! compilation unit ([`DebugContext`]).
//!
//! A variable is told of when it is kept in one slot of the frame for as long as the function
//! runs ([`VariableDebug`]), by where the slot is from the frame pointer.

mod codeview;
mod set;
mod split;
mod types;

use codeview::CodeView;
pub(super) use set::TypeSet;
use set::{Of, Told};
pub(super) use types::{ByEnv, Sets, described};

use cranelift_codegen::{
    binemit::CodeOffset,
    gimli::{
        self, DW_AT_GNU_dwo_id, DW_AT_GNU_dwo_name, DW_AT_artificial, DW_AT_byte_size,
        DW_AT_comp_dir, DW_AT_const_value, DW_AT_count, DW_AT_data_member_location,
        DW_AT_decl_file, DW_AT_decl_line, DW_AT_discr, DW_AT_discr_value, DW_AT_encoding,
        DW_AT_enum_class, DW_AT_frame_base, DW_AT_high_pc, DW_AT_language, DW_AT_linkage_name,
        DW_AT_location, DW_AT_low_pc, DW_AT_name, DW_AT_producer, DW_AT_ranges, DW_AT_stmt_list,
        DW_AT_type, DW_TAG_array_type, DW_TAG_base_type, DW_TAG_enumeration_type,
        DW_TAG_enumerator, DW_TAG_formal_parameter, DW_TAG_lexical_block, DW_TAG_member,
        DW_TAG_namespace, DW_TAG_pointer_type, DW_TAG_structure_type, DW_TAG_subprogram,
        DW_TAG_subrange_type, DW_TAG_template_type_parameter, DW_TAG_union_type, DW_TAG_variable,
        DW_TAG_variant, DW_TAG_variant_part, DwAte, Encoding, Format, LineEncoding, RunTimeEndian,
        SectionId,
        write::{
            Address, AttributeValue, DwarfUnit, EndianVec, Expression, FileId, LineProgram,
            LineString, Location, LocationList, Range, RangeList, Result as WriteResult, Sections,
            UnitEntryId, Writer,
        },
    },
    ir,
    isa::TargetIsa,
};
use cranelift_module::FuncId;
use cranelift_object::{
    ObjectProduct,
    object::{
        BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SectionKind,
        write::{Relocation, StandardSegment, SymbolId},
    },
};
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHasher};
use serde_derive::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// The statement the code of a function is of from `offset` on, as an index into the
/// statements the function was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct LineRow {
    pub(super) offset: CodeOffset,
    pub(super) span: u32,
}

impl LineRow {
    /// The first source location that stands for a statement: 0 is that of none.
    const FIRST: u32 = 1;

    /// The source location of instructions built for the statement of index `span`.
    pub(super) fn source_loc(span: u32) -> ir::SourceLoc {
        ir::SourceLoc::new(span + Self::FIRST)
    }

    fn span_of(loc: ir::SourceLoc) -> Option<u32> {
        super::unwind::seh::unguarded(loc).bits().checked_sub(Self::FIRST)
    }

    /// The rows of a function from the source location its code has from each offset on, in
    /// the order of the offsets. Code of no statement goes with the statement before it.
    pub(super) fn of_ranges(
        ranges: impl IntoIterator<Item = (CodeOffset, ir::SourceLoc)>,
    ) -> Vec<LineRow> {
        let mut rows: Vec<LineRow> = Vec::new();
        for (offset, loc) in ranges {
            let Some(span) = Self::span_of(loc) else { continue };
            if rows.last().is_none_or(|last| last.span != span) {
                rows.push(LineRow { offset, span });
            }
        }
        rows
    }
}

/// A line of a source file, which is an index into the files of a [`FunctionDebug`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Place {
    pub(super) file: u32,
    pub(super) line: u32,
    pub(super) column: u32,
}

/// A type among the types of a list of [`DebugType`]s, by its place in it.
pub(super) type TypeIndex = u32;

/// What a debugger is told of a type: its name, its size, and what it is made of as far as
/// that is told. A type names the types it is made of by their place in the same list, so
/// one that points to itself can be told of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum DebugType {
    /// A number, a `bool` or a `char`, whose bytes are read as the `DW_ATE_*` of `encoding`.
    Base {
        name: String,
        size: u64,
        encoding: u8,
    },
    /// A pointer of one word. `to` is `None` where what it points to is not told.
    Pointer {
        name: String,
        to: Option<TypeIndex>,
    },
    Array {
        of: TypeIndex,
        len: u64,
    },
    /// A struct, a tuple, a closure or a pointer of two words. One without fields may have
    /// some that are not told.
    Struct(Aggregate),
    Union(Aggregate),
    /// An enum none of whose variants has a field: a number with a name for each value.
    Names {
        name: String,
        size: u64,
        names: Vec<(String, u64)>,
    },
    /// An enum with fields. `tag` is `None` where there is one variant and nothing says which.
    Enum {
        of: Aggregate,
        tag: Option<Tag>,
        variants: Vec<Variant>,
    },
}

impl DebugType {
    /// How many types inside each other a debugger is told of.
    pub(super) const DEEPEST: u32 = 128;
}

/// A name, a size, the fields and the types the generic parameters stand for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Aggregate {
    pub(super) name: String,
    pub(super) size: u64,
    pub(super) fields: Vec<Field>,
    pub(super) parameters: Vec<(String, TypeIndex)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Field {
    pub(super) name: String,
    pub(super) offset: u64,
    pub(super) ty: TypeIndex,
}

/// Where an enum keeps which variant a value is of: an unsigned number of `size` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct Tag {
    pub(super) offset: u64,
    pub(super) size: u64,
    /// Whether the variants are the states of a coroutine, which rustc tells of by their
    /// numbers, with a name for the tag.
    pub(super) states: bool,
}

/// A variant of an enum. `tag` is the value of the tag that says a value is of this variant;
/// the variant without one is what every other value of the tag means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Variant {
    pub(super) name: String,
    pub(super) tag: Option<u64>,
    pub(super) fields: Vec<Field>,
}

/// Where the value of a variable is while the function runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum VariablePlace {
    /// In the frame, this far from what the frame is found by, from the first instruction
    /// to the last.
    Frame(i64),
    /// What a DWARF expression says, for the code from the first offset up to the second.
    /// Code of no range has the value nowhere a debugger can read it.
    Ranges(Vec<(CodeOffset, CodeOffset, Vec<u8>)>),
}

/// Where the stack slots of a function are while it runs, for the ones that hold a local
/// written with a name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct SlotPlaces {
    /// The DWARF expression of what [`VariablePlace::Frame`] and `DW_OP_fbreg` count from.
    pub(super) frame_base: Vec<u8>,
    /// The number of a slot and where its bytes are.
    pub(super) slots: Vec<(u32, VariablePlace)>,
}

impl SlotPlaces {
    /// The places in a frame Cranelift laid out for `func`, which it finds by the frame
    /// pointer.
    pub(super) fn of_frame(
        isa: &dyn TargetIsa,
        frame: &cranelift_codegen::MachBufferFrameLayout,
        func: &ir::Function,
        slots: impl Iterator<Item = ir::StackSlot>,
    ) -> SlotPlaces {
        // The register a function with locals has its frame's address in, which is what
        // Cranelift says where they are from.
        let register = match isa.triple().architecture {
            target_lexicon::Architecture::X86_64 => gimli::X86_64::RBP,
            target_lexicon::Architecture::Aarch64(_) => gimli::AArch64::X29,
            _ => return SlotPlaces::default(),
        };
        let to_fp = i64::from(frame.frame_to_fp_offset);
        // Every slot of the function is in the layout, but one read back from Cranelift's
        // incremental cache leaves out the slots at its end that are at the bottom of the
        // frame, as a `SecondaryMap` written out leaves out its default entries (#242).
        let slots = slots
            .filter(|&slot| func.sized_stack_slots.is_valid(slot))
            .map(|slot| {
                let offset = i64::from(frame.stackslots[slot].offset) - to_fp;
                (slot.as_u32(), VariablePlace::Frame(offset))
            })
            .collect();
        SlotPlaces { frame_base: vec![gimli::DW_OP_reg0.0 + register.0 as u8], slots }
    }
}

impl VariablePlace {
    /// Where the value is that is `offset` bytes from the address this place holds. Code a
    /// debugger cannot read the address in has no place for the value.
    pub(super) fn pointee(&self, offset: u32) -> VariablePlace {
        let ranges = match self {
            VariablePlace::Frame(offset) => {
                let mut held = vec![gimli::DW_OP_fbreg.0];
                // An expression of one operation writes nowhere it can fail.
                let _ = gimli::leb128::write::signed(&mut held, *offset);
                vec![(0, CodeOffset::MAX, held)]
            }
            VariablePlace::Ranges(ranges) => ranges.clone(),
        };
        let ranges = ranges
            .into_iter()
            .filter_map(|(from, to, held)| Some((from, to, pointee_expression(&held, offset)?)))
            .collect();
        VariablePlace::Ranges(ranges)
    }
}

/// The expression of the memory `offset` bytes from an address, from the one of where the
/// address is held: in a register, or in memory a single operation finds.
fn pointee_expression(held: &[u8], offset: u32) -> Option<Vec<u8>> {
    let (&operation, operands) = held.split_first()?;
    let register = gimli::DW_OP_reg0.0..=gimli::DW_OP_reg31.0;
    let based = gimli::DW_OP_breg0.0..=gimli::DW_OP_breg31.0;
    let mut pointee = if register.contains(&operation) && operands.is_empty() {
        vec![operation - gimli::DW_OP_reg0.0 + gimli::DW_OP_breg0.0, 0]
    } else if operation == gimli::DW_OP_fbreg.0 || based.contains(&operation) {
        // The one operand is a LEB128 number, which ends at the first byte without the high
        // bit.
        let end = operands.iter().position(|byte| byte & 0x80 == 0)?;
        if end + 1 != operands.len() {
            return None;
        }
        [held, &[gimli::DW_OP_deref.0]].concat()
    } else {
        return None;
    };
    if offset != 0 {
        pointee.push(gimli::DW_OP_plus_uconst.0);
        gimli::leb128::write::unsigned(&mut pointee, u64::from(offset)).ok()?;
    }
    Some(pointee)
}

impl SlotPlaces {
    pub(super) fn of(&self, slot: u32) -> Option<&VariablePlace> {
        self.slots.iter().find(|it| it.0 == slot).map(|it| &it.1)
    }
}

/// A part of a function its names are looked up in: the body, a block, what follows a `let`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Scope {
    /// The scope this one is inside of, by its place among the scopes. It comes before.
    pub(super) parent: Option<u32>,
    /// The code of the scope and of the scopes inside it, in the order of the offsets.
    pub(super) ranges: Vec<(CodeOffset, CodeOffset)>,
}

/// A parameter or a `let` of a function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct VariableDebug {
    pub(super) name: String,
    pub(super) param: bool,
    /// Which of the [`FunctionDebug::types`] the type is the first of.
    pub(super) ty: u32,
    pub(super) place: VariablePlace,
    /// The scope the name is known in, by its place among the scopes: `None` is all of the
    /// function.
    pub(super) scope: Option<u32>,
}

/// What a debugger is told about one function, with nothing in it that depends on where the
/// function ends up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FunctionDebug {
    /// The path a debugger knows the function by, as in `a::B<c::D>::e<F>`.
    pub(super) name: String,
    /// What a backtrace calls the function: its symbol when that is one rustc mangled, and
    /// otherwise its path the way such a symbol demangles.
    pub(super) linkage_name: String,
    /// The files as rustc names them, relative to the checkout when they are of it.
    pub(super) files: Vec<String>,
    pub(super) declared: Option<Place>,
    /// Where the code is from, from each offset on, in the order of the offsets.
    pub(super) rows: Vec<(CodeOffset, Place)>,
    /// The types of the variables and what those are made of. The store keeps a set once,
    /// for every function that has it: see `stored_types`.
    #[serde(skip)]
    pub(super) types: Vec<TypeSet>,
    /// The keys `types` are in the store under, of an entry of the store.
    pub(super) stored_types: Vec<[u8; super::store::KEY]>,
    pub(super) scopes: Vec<Scope>,
    pub(super) variables: Vec<VariableDebug>,
    /// The DWARF expression of what [`VariablePlace::Frame`] and `DW_OP_fbreg` count from.
    pub(super) frame_base: Vec<u8>,
}

impl FunctionDebug {
    pub(super) fn new(name: String, linkage_name: String) -> Self {
        FunctionDebug {
            name,
            linkage_name,
            files: Vec::new(),
            declared: None,
            rows: Vec::new(),
            types: Vec::new(),
            stored_types: Vec::new(),
            scopes: Vec::new(),
            variables: Vec::new(),
            frame_base: Vec::new(),
        }
    }

    /// The place of `line` and `column` in the file rustc calls `file`.
    pub(super) fn place(&mut self, file: String, line: u32, column: u32) -> Place {
        let known = self.files.iter().position(|it| *it == file);
        let file = known.unwrap_or_else(|| {
            self.files.push(file);
            self.files.len() - 1
        });
        Place { file: file as u32, line, column }
    }

    /// Adds that the code from `offset` on is of `place`.
    pub(super) fn row(&mut self, offset: CodeOffset, place: Place) {
        if self.rows.last().is_none_or(|last| last.1 != place) {
            self.rows.push((offset, place));
        }
    }
}

/// What a debugger is told of the functions of an object, in the tables its format has.
pub(super) enum DebugContext {
    Dwarf(Box<Dwarf>),
    CodeView(CodeView),
}

impl DebugContext {
    /// The tables of an object for `isa` whose files are named from `directory`, or `None`
    /// where they are not written yet: ELF and Mach-O have DWARF and COFF has CodeView.
    /// DWARF leaves the variables of the functions out unless `variables` asks for them, and
    /// on ELF keeps its entries apart from what is linked when `apart` names the file a
    /// debugger reads them from; Mach-O links none of it, and CodeView has neither yet.
    /// With `grouped`, each unit linked on ELF has sections of its own (see
    /// [`Dwarf::sections`]).
    pub(super) fn new(
        isa: &dyn TargetIsa,
        directory: &str,
        variables: bool,
        apart: Option<String>,
        grouped: bool,
    ) -> Option<Self> {
        if super::Target::current().is_wasm() {
            return None;
        }
        match isa.triple().binary_format {
            target_lexicon::BinaryFormat::Elf => {
                let mut dwarf = Dwarf::new(isa, directory, variables, apart);
                dwarf.grouped = grouped;
                Some(DebugContext::Dwarf(Box::new(dwarf)))
            }
            target_lexicon::BinaryFormat::Macho => {
                let dwarf = Dwarf::new(isa, directory, variables, None);
                Some(DebugContext::Dwarf(Box::new(dwarf)))
            }
            target_lexicon::BinaryFormat::Coff => {
                Some(DebugContext::CodeView(CodeView::new(directory)))
            }
            _ => None,
        }
    }

    /// Describes the function `func` of the module, which is `len` bytes of code.
    pub(super) fn add_function(&mut self, func: FuncId, len: u32, debug: &FunctionDebug) {
        match self {
            DebugContext::Dwarf(dwarf) => dwarf.add_function(func, len, debug),
            DebugContext::CodeView(codeview) => codeview.add_function(func, len, debug),
        }
    }

    /// Writes the sections into the object, when it has a function to describe. What is
    /// returned is what was kept apart from the object.
    pub(super) fn emit(self, product: &mut ObjectProduct) -> Result<Vec<Sidecar>, String> {
        match self {
            DebugContext::Dwarf(dwarf) => dwarf.emit(product),
            DebugContext::CodeView(codeview) => codeview.emit(product).map(|()| Vec::new()),
        }
    }
}

/// A [`DebugContext`] on threads of their own, which describe the functions they are sent
/// while the thread that sends them goes on putting code in the object. DWARF linked into
/// an ELF object is described on several, each writing units of its own: one thread falls
/// behind the one putting code in the object, and the object waits for it.
pub(super) struct Describer {
    shards: Vec<Shard>,
    /// How many functions were sent, which picks the thread a function goes to.
    sent: std::sync::atomic::AtomicUsize,
}

/// A function to describe, with the entries it is of when the store had it: its types are read
/// on the describing thread.
type Sent = (FuncId, u32, FunctionDebug, Option<std::sync::Arc<super::cached::Entries>>);

struct Shard {
    functions: std::sync::mpsc::Sender<Sent>,
    described: std::thread::JoinHandle<Described>,
}

/// What a [`Describer`] ends with.
enum Described {
    /// The sections, written already.
    Sections(Result<Vec<Section>, String>),
    /// What writes them once it has the object to look into.
    Context(DebugContext),
}

impl Describer {
    /// With `elf`, the object is an ELF one, which can have DWARF of several threads: its
    /// linker puts the sections of one name together, and each unit reads the sections
    /// beside it.
    pub(super) fn new(context: DebugContext, elf: bool) -> Describer {
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
        let mut contexts = vec![];
        if let DebugContext::Dwarf(dwarf) = &context
            && elf
            && dwarf.apart.is_none()
            && !dwarf.grouped
        {
            // The types of the functions put in from the store are read here as well.
            contexts.extend((1..(threads / 4).clamp(1, 16)).map(|_| dwarf.like()));
        }
        let contexts = std::iter::once(context)
            .chain(contexts.into_iter().map(|it| DebugContext::Dwarf(Box::new(it))));
        let shards = contexts.map(Shard::new).collect();
        Describer { shards, sent: Default::default() }
    }

    /// Describes the function `func` of the module, which is `len` bytes of code.
    pub(super) fn add_function(&self, func: FuncId, len: u32, debug: FunctionDebug) {
        self.send((func, len, debug, None));
    }

    /// Describes the function `func` put in the module from `entries`.
    pub(super) fn add_stored(
        &self,
        func: FuncId,
        len: u32,
        debug: FunctionDebug,
        entries: std::sync::Arc<super::cached::Entries>,
    ) {
        self.send((func, len, debug, Some(entries)));
    }

    fn send(&self, function: Sent) {
        let sent = self.sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let shard = &self.shards[(sent / FUNCTIONS_OF_A_UNIT) % self.shards.len()];
        // The thread ends only when it panicked, which `emit` says.
        let _ = shard.functions.send(function);
    }

    /// Writes the sections into the object, when it has a function to describe. What is
    /// returned is what was kept apart from the object.
    pub(super) fn emit(self, product: &mut ObjectProduct) -> Result<Vec<Sidecar>, String> {
        let mut written = Vec::new();
        let mut kept = None;
        // Every thread is told there is nothing more before any is waited for.
        let threads: Vec<_> = self.shards.into_iter().map(|it| it.described).collect();
        for described in threads {
            match described.join().map_err(|_| "describing the functions panicked")? {
                Described::Sections(sections) => written.push(sections?),
                Described::Context(context) => kept = Some(context),
            }
        }
        if let Some(context) = kept {
            return context.emit(product);
        }
        if matches!(product.object.format(), BinaryFormat::Elf | BinaryFormat::MachO) {
            place_all(product, written)?;
        }
        Ok(Vec::new())
    }
}

impl Shard {
    fn new(mut context: DebugContext) -> Shard {
        let (functions, sent) = std::sync::mpsc::channel::<Sent>();
        let described = std::thread::spawn(move || {
            for (func, len, mut debug, entries) in sent {
                if let Some(entries) = entries {
                    entries.describe(&mut debug);
                }
                context.add_function(func, len, &debug);
            }
            match context {
                DebugContext::Dwarf(dwarf) if dwarf.apart.is_none() => {
                    Described::Sections(dwarf.sections())
                }
                context => Described::Context(context),
            }
        });
        Shard { functions, described }
    }
}

/// How many functions a compilation unit describes before the next one is begun. What reads
/// a unit reads all of it to find one function, so a unit of every function of a large
/// object takes a debugger or `addr2line` a second to answer from.
const FUNCTIONS_OF_A_UNIT: usize = 256;

/// The compilation units of an object, which every function of the object is described in.
pub(super) struct Dwarf {
    /// The unit functions are being added to.
    dwarf: DwarfUnit,
    /// The units that have all their functions, written as each is full: their entries,
    /// kept until the object was written, held gigabytes of a large program's build.
    full: Written,
    /// What the names of the files the entries of the units are read from start with, when
    /// they are kept apart from what is linked, and the units that are.
    apart: Option<(String, Vec<split::Apart>)>,
    /// What tells the unit functions are being added to from another: what was added.
    id: FxHasher,
    encoding: Encoding,
    directory: Vec<u8>,
    files: FxHashMap<String, FileId>,
    ranges: Vec<Range>,
    /// The addresses the entries of a unit kept apart name by their place in this list.
    addresses: Vec<Address>,
    types: FxHashMap<SharedType, UnitEntryId>,
    /// The first entry of each name among `types`.
    names: FxHashMap<Symbol, UnitEntryId>,
    /// The entry of the type each set seen is of, by where the set is, with the set so that
    /// nothing else comes to be there.
    sets: FxHashMap<usize, (TypeSet, Option<UnitEntryId>)>,
    /// The modules and functions of the unit that a type is written in, by their paths.
    namespaces: FxHashMap<String, UnitEntryId>,
    /// Whether the variables of the functions are told of, and not their lines alone.
    variables: bool,
    /// Whether each unit is written in sections of its own.
    grouped: bool,
}

/// What tells one type of the unit from another: every function with a variable of the
/// type shares its entry.
#[derive(PartialEq, Eq, Hash)]
enum SharedType {
    Named(gimli::DwTag, Symbol, u64),
    Array(UnitEntryId, u64),
}

/// The entries of the types of one set, as far as they have been asked for.
#[derive(Default)]
struct FunctionTypes {
    /// By where each type's set is.
    entries: FxHashMap<usize, UnitEntryId>,
    /// How many types the one being told of is inside of.
    inside: u32,
}

impl Dwarf {
    /// The unit of an object for `isa` whose files are named from `directory`. The
    /// variables of the functions are left out unless `variables` asks for them. Their
    /// entries are kept apart from what is linked when `apart` names the file a debugger
    /// reads them from.
    fn new(isa: &dyn TargetIsa, directory: &str, variables: bool, apart: Option<String>) -> Self {
        let encoding =
            Encoding { format: Format::Dwarf32, version: 4, address_size: isa.pointer_bytes() };
        Self::of(encoding, directory.as_bytes().to_vec(), variables, apart)
    }

    /// Units of their own, of the object this describes the functions of.
    fn like(&self) -> Self {
        let apart = self.apart.as_ref().map(|it| it.0.clone());
        let mut dwarf = Self::of(self.encoding, self.directory.clone(), self.variables, apart);
        dwarf.grouped = self.grouped;
        dwarf
    }

    fn of(encoding: Encoding, directory: Vec<u8>, variables: bool, apart: Option<String>) -> Self {
        let mut context = Dwarf {
            dwarf: DwarfUnit::new(encoding),
            full: Written::default(),
            apart: apart.map(|name| (name, Vec::new())),
            id: FxHasher::default(),
            addresses: Vec::new(),
            encoding,
            directory,
            files: FxHashMap::default(),
            ranges: Vec::new(),
            types: FxHashMap::default(),
            names: FxHashMap::default(),
            sets: FxHashMap::default(),
            namespaces: FxHashMap::default(),
            variables,
            grouped: false,
        };
        context.begin_unit();
        context
    }

    /// Sets up the unit functions are added to, which is a new one.
    fn begin_unit(&mut self) {
        let dwarf = &mut self.dwarf;
        dwarf.unit.line_program = LineProgram::new(
            self.encoding,
            LineEncoding::default(),
            LineString::String(self.directory.clone()),
            None,
            LineString::String(b"<blaze>".to_vec()),
            None,
        );
        // No version: the release blaze is is told only to the engine, so that a release changes
        // no crate below it (crates/blaze-engine/build.rs).
        let producer = self.text(b"blaze");
        let name = self.text(b"<blaze>");
        let directory = self.text(&self.directory.clone());
        let unit = &mut self.dwarf.unit;
        let root = unit.get_mut(unit.root());
        root.set(DW_AT_producer, producer);
        root.set(DW_AT_language, AttributeValue::Language(gimli::DW_LANG_Rust));
        root.set(DW_AT_name, name);
        // Where the code and its lines are is the skeleton's to say of a unit kept apart.
        if self.apart.is_none() {
            root.set(DW_AT_comp_dir, directory);
            root.set(DW_AT_stmt_list, AttributeValue::LineProgramRef);
            root.set(DW_AT_low_pc, AttributeValue::Address(Address::Constant(0)));
        }
    }

    /// A name as an entry has it: among the strings of the object, or in the entry itself
    /// when it is kept apart, where there are no strings to share.
    fn text(&mut self, text: &[u8]) -> AttributeValue {
        match self.apart {
            Some(_) => AttributeValue::String(text.to_vec()),
            None => AttributeValue::StringRef(self.dwarf.strings.add(text)),
        }
    }

    /// Ends the unit functions were added to, which is left out when it has none, and
    /// begins the next.
    fn end_unit(&mut self) {
        let next = gimli::write::Unit::new(self.encoding, LineProgram::none());
        let mut unit = std::mem::replace(&mut self.dwarf.unit, next);
        let ranges = std::mem::take(&mut self.ranges);
        let id = std::mem::take(&mut self.id).finish();
        self.files.clear();
        self.types.clear();
        self.names.clear();
        self.sets.clear();
        self.namespaces.clear();
        self.begin_unit();
        if ranges.is_empty() {
            return;
        }
        let root = unit.root();
        let addresses = std::mem::take(&mut self.addresses);
        if let Some((name, apart)) = &mut self.apart {
            let lines = std::mem::replace(&mut unit.line_program, LineProgram::none());
            let root = unit.get_mut(root);
            let name = split::file(name, apart.len()).into_bytes();
            root.set(DW_AT_GNU_dwo_name, AttributeValue::String(name));
            root.set(DW_AT_GNU_dwo_id, AttributeValue::Data8(id));
            apart.push(split::Apart { unit, lines, ranges, addresses, id });
            return;
        }
        let ranges = unit.ranges.add(RangeList(ranges));
        unit.get_mut(root).set(DW_AT_ranges, AttributeValue::RangeListRef(ranges));
        if let Err(why) = self.write_unit(unit) {
            self.full.failed.get_or_insert(why);
        }
    }

    /// Writes a unit that has all its functions: into the sections every unit shares, or
    /// into sections of its own when grouped. The strings it names are written last, once
    /// every unit has added its own.
    fn write_unit(&mut self, unit: gimli::write::Unit) -> Result<(), String> {
        let mut units = gimli::write::UnitTable::default();
        units.add(unit);
        let (line_strings, strings) = (&mut self.dwarf.line_strings, &mut self.dwarf.strings);
        let full = &mut self.full;
        full.units += 1;
        if !self.grouped {
            return units.write(&mut full.shared, line_strings, strings).map_err(|e| e.to_string());
        }
        let mut out = Sections::new(SectionWriter::default());
        units.write(&mut out, line_strings, strings).map_err(|e| e.to_string())?;
        let group = Some(full.units - 1);
        out.for_each_mut(|id, section: &mut SectionWriter| {
            full.grouped.push(Section { id, group, written: std::mem::take(section) });
            Ok::<_, String>(())
        })
    }

    fn file(&mut self, name: &str) -> FileId {
        if let Some(&id) = self.files.get(name) {
            return id;
        }
        let program = &mut self.dwarf.unit.line_program;
        let directory = program.default_directory();
        let id = program.add_file(LineString::String(name.as_bytes().to_vec()), directory, None);
        self.files.insert(name.to_owned(), id);
        id
    }

    /// The entry of the type a set is of, which is found once for the unit.
    fn set(&mut self, types: &TypeSet) -> Option<UnitEntryId> {
        let at = types.at();
        if let Some((_, entry)) = self.sets.get(&at) {
            return *entry;
        }
        let entry = self.ty(&mut FunctionTypes::default(), &Of::Type(types.clone()));
        self.sets.insert(at, (types.clone(), entry));
        entry
    }

    /// The entry of the type `part` is. A type inside more than [`DebugType::DEEPEST`]
    /// others is not told of: the types of an entry read from the store are not known to be
    /// what a compiler would have written, and each one inside another takes a few frames of
    /// the stack.
    fn ty(&mut self, of: &mut FunctionTypes, part: &Of<'_>) -> Option<UnitEntryId> {
        let set = match part {
            Of::Type(set) => set,
            Of::Named(name) => return self.names.get(*name).copied(),
        };
        if let Some(&entry) = of.entries.get(&set.at()) {
            return Some(entry);
        }
        if of.inside == DebugType::DEEPEST {
            return None;
        }
        of.inside += 1;
        let entry = self.new_type(of, set);
        of.inside -= 1;
        entry
    }

    /// The entry of the type of `set`, which has none yet.
    fn new_type(&mut self, of: &mut FunctionTypes, set: &TypeSet) -> Option<UnitEntryId> {
        let ty: &Told = &set.node().told;
        let made_of: Vec<Of<'_>> = set.of().collect();
        let shared = match ty {
            Told::Base { name, size, .. } => {
                SharedType::Named(DW_TAG_base_type, name.clone(), *size)
            }
            Told::Pointer { name } => SharedType::Named(DW_TAG_pointer_type, name.clone(), 0),
            Told::Array { len } => SharedType::Array(self.ty(of, made_of.first()?)?, *len),
            Told::Struct(it) | Told::Enum { of: it, .. } => {
                SharedType::Named(DW_TAG_structure_type, it.name.clone(), it.size)
            }
            Told::Union(it) => SharedType::Named(DW_TAG_union_type, it.name.clone(), it.size),
            Told::Names { name, size, .. } => {
                SharedType::Named(DW_TAG_enumeration_type, name.clone(), *size)
            }
        };
        if let Some(&entry) = self.types.get(&shared) {
            of.entries.insert(set.at(), entry);
            // A function that followed fewer pointers to the type left it without what it
            // points to.
            if let Told::Pointer { .. } = ty
                && let Some(to) = made_of.first()
                && self.dwarf.unit.get(entry).get(DW_AT_type).is_none()
                && let Some(to) = self.ty(of, to)
            {
                self.dwarf.unit.get_mut(entry).set(DW_AT_type, AttributeValue::UnitRef(to));
            }
            return Some(entry);
        }
        let entry = match &shared {
            SharedType::Named(tag, name, _) => {
                let (within, name) = match ty {
                    Told::Base { .. } | Told::Pointer { .. } => (None, name.as_str()),
                    _ => within(name.as_str()),
                };
                let parent = self.namespace(within);
                self.named(parent, *tag, name)
            }
            SharedType::Array(..) => self.dwarf.unit.add(self.dwarf.unit.root(), DW_TAG_array_type),
        };
        of.entries.insert(set.at(), entry);
        if let SharedType::Named(_, name, _) = &shared {
            self.names.entry(name.clone()).or_insert(entry);
        }
        self.types.insert(shared, entry);
        // A type made of itself finds its entry here.
        self.fill(of, entry, ty, &made_of);
        Some(entry)
    }

    /// The entry of the module or function at `path`, which a debugger puts in front of the
    /// names of the types in it, as rustc has them. No path is the unit itself.
    fn namespace(&mut self, path: Option<&str>) -> UnitEntryId {
        let Some(path) = path else { return self.dwarf.unit.root() };
        if let Some(&entry) = self.namespaces.get(path) {
            return entry;
        }
        let (outer, name) = last_of(path);
        let parent = self.namespace(outer);
        let entry = self.named(parent, DW_TAG_namespace, name);
        self.namespaces.insert(path.to_owned(), entry);
        entry
    }

    /// Writes what `ty` is made of into its `entry`: the types at `made_of`, in the order
    /// [`Told`] says.
    fn fill(&mut self, of: &mut FunctionTypes, entry: UnitEntryId, ty: &Told, made_of: &[Of<'_>]) {
        let size = |unit: &mut gimli::write::Unit, size: u64| {
            unit.get_mut(entry).set(DW_AT_byte_size, AttributeValue::Udata(size));
        };
        match ty {
            Told::Base { size: bytes, encoding, .. } => {
                size(&mut self.dwarf.unit, *bytes);
                let encoding = AttributeValue::Encoding(DwAte(*encoding));
                self.dwarf.unit.get_mut(entry).set(DW_AT_encoding, encoding);
            }
            Told::Pointer { .. } => {
                if let Some(to) = made_of.first().and_then(|to| self.ty(of, to)) {
                    self.dwarf.unit.get_mut(entry).set(DW_AT_type, AttributeValue::UnitRef(to));
                }
            }
            Told::Array { len } => {
                let Some(element) = made_of.first().and_then(|it| self.ty(of, it)) else {
                    return;
                };
                let unit = &mut self.dwarf.unit;
                unit.get_mut(entry).set(DW_AT_type, AttributeValue::UnitRef(element));
                let range = unit.add(entry, DW_TAG_subrange_type);
                unit.get_mut(range).set(DW_AT_count, AttributeValue::Udata(*len));
            }
            Told::Struct(it) | Told::Union(it) => {
                size(&mut self.dwarf.unit, it.size);
                let (fields, parameters) = made_of.split_at(it.fields.len());
                self.members(of, entry, &it.fields, fields);
                self.parameters(of, entry, &it.parameters, parameters);
            }
            Told::Names { size: bytes, names, .. } => {
                size(&mut self.dwarf.unit, *bytes);
                let number = self.unsigned(*bytes);
                let unit = self.dwarf.unit.get_mut(entry);
                unit.set(DW_AT_type, AttributeValue::UnitRef(number));
                unit.set(DW_AT_enum_class, AttributeValue::Flag(true));
                for (name, value) in names {
                    let name = self.named(entry, DW_TAG_enumerator, name.as_str());
                    let value = AttributeValue::Udata(*value);
                    self.dwarf.unit.get_mut(name).set(DW_AT_const_value, value);
                }
            }
            Told::Enum { of: it, tag, variants } => {
                size(&mut self.dwarf.unit, it.size);
                self.variants(of, entry, it, *tag, variants, &made_of[it.fields.len()..]);
            }
        }
    }

    /// The variants of the enum at `entry` the way rustc tells of them: a part of the enum
    /// that is one of several members by the value of another, each member a struct of its
    /// own with the fields of a variant, and with what the generic parameters of the enum
    /// stand for: on the enum itself they would have lldb spell its name its own way.
    /// `made_of` is what the generic parameters stand for, then the types of the fields of
    /// each variant.
    fn variants(
        &mut self,
        of: &mut FunctionTypes,
        entry: UnitEntryId,
        whole: &set::Shape,
        tag: Option<Tag>,
        variants: &[set::Shaped],
        made_of: &[Of<'_>],
    ) {
        let (parameters, mut left) = made_of.split_at(whole.parameters.len());
        let part = self.dwarf.unit.add(entry, DW_TAG_variant_part);
        if let Some(tag) = tag {
            let number = self.unsigned(tag.size);
            let unit = &mut self.dwarf.unit;
            let member = unit.add(part, DW_TAG_member);
            let entry = unit.get_mut(member);
            entry.set(DW_AT_type, AttributeValue::UnitRef(number));
            entry.set(DW_AT_artificial, AttributeValue::Flag(true));
            entry.set(DW_AT_data_member_location, AttributeValue::Udata(tag.offset));
            unit.get_mut(part).set(DW_AT_discr, AttributeValue::UnitRef(member));
            if tag.states {
                let name = self.text(b"__state");
                self.dwarf.unit.get_mut(member).set(DW_AT_name, name);
            }
        }
        let states = tag.is_some_and(|it| it.states);
        for variant in variants {
            let types;
            (types, left) = left.split_at(variant.fields.len());
            let fields = self.named(entry, DW_TAG_structure_type, variant.name.as_str());
            let size = AttributeValue::Udata(whole.size);
            self.dwarf.unit.get_mut(fields).set(DW_AT_byte_size, size);
            self.parameters(of, fields, &whole.parameters, parameters);
            self.members(of, fields, &variant.fields, types);
            let choice = self.dwarf.unit.add(part, DW_TAG_variant);
            if let Some(value) = variant.tag {
                let value = AttributeValue::Udata(value);
                self.dwarf.unit.get_mut(choice).set(DW_AT_discr_value, value);
            }
            let member = match variant.tag {
                Some(state) if states => self.named(choice, DW_TAG_member, &state.to_string()),
                _ => self.named(choice, DW_TAG_member, variant.name.as_str()),
            };
            let member = self.dwarf.unit.get_mut(member);
            member.set(DW_AT_type, AttributeValue::UnitRef(fields));
            member.set(DW_AT_data_member_location, AttributeValue::Udata(0));
        }
    }

    /// The fields of the type at `entry`, by their names and offsets, of the types at `types`.
    fn members(
        &mut self,
        of: &mut FunctionTypes,
        entry: UnitEntryId,
        fields: &[(Symbol, u64)],
        types: &[Of<'_>],
    ) {
        for ((name, offset), ty) in fields.iter().zip(types) {
            let Some(ty) = self.ty(of, ty) else { continue };
            let member = self.named(entry, DW_TAG_member, name.as_str());
            let member = self.dwarf.unit.get_mut(member);
            member.set(DW_AT_type, AttributeValue::UnitRef(ty));
            member.set(DW_AT_data_member_location, AttributeValue::Udata(*offset));
        }
    }

    /// The types the generic parameters of the type at `entry` stand for, which a
    /// pretty-printer finds what a `Vec` holds by: those at `types`.
    fn parameters(
        &mut self,
        of: &mut FunctionTypes,
        entry: UnitEntryId,
        parameters: &[Symbol],
        types: &[Of<'_>],
    ) {
        for (name, ty) in parameters.iter().zip(types) {
            let Some(ty) = self.ty(of, ty) else { continue };
            let parameter = self.named(entry, DW_TAG_template_type_parameter, name.as_str());
            self.dwarf.unit.get_mut(parameter).set(DW_AT_type, AttributeValue::UnitRef(ty));
        }
    }

    /// The unsigned number of `size` bytes.
    fn unsigned(&mut self, size: u64) -> UnitEntryId {
        let name = format!("u{}", size * 8);
        let shared = SharedType::Named(DW_TAG_base_type, Symbol::intern(&name), size);
        if let Some(&entry) = self.types.get(&shared) {
            return entry;
        }
        let entry = self.named(self.dwarf.unit.root(), DW_TAG_base_type, &name);
        let unit = self.dwarf.unit.get_mut(entry);
        unit.set(DW_AT_byte_size, AttributeValue::Udata(size));
        unit.set(DW_AT_encoding, AttributeValue::Encoding(gimli::DW_ATE_unsigned));
        self.names.entry(Symbol::intern(&name)).or_insert(entry);
        self.types.insert(shared, entry);
        entry
    }

    fn named(&mut self, parent: UnitEntryId, tag: gimli::DwTag, name: &str) -> UnitEntryId {
        let name = self.text(name.as_bytes());
        let entry = self.dwarf.unit.add(parent, tag);
        self.dwarf.unit.get_mut(entry).set(DW_AT_name, name);
        entry
    }

    /// Tells of the variables of the function at `function`, which starts at `start` and is
    /// `len` bytes of code.
    fn variables(
        &mut self,
        function: UnitEntryId,
        start: Address,
        len: u32,
        debug: &FunctionDebug,
    ) {
        if !self.variables || debug.variables.is_empty() || debug.frame_base.is_empty() {
            return;
        }
        let base = Expression::raw(debug.frame_base.clone());
        self.dwarf.unit.get_mut(function).set(DW_AT_frame_base, AttributeValue::Exprloc(base));
        let mut blocks = vec![None; debug.scopes.len()];
        for variable in &debug.variables {
            let Some(types) = debug.types.get(variable.ty as usize) else { continue };
            let Some(ty) = self.set(types) else { continue };
            let parent = match variable.scope {
                None => Some(function),
                Some(scope) => self.block(function, start, debug, &mut blocks, scope),
            };
            // A name of a scope that no code is of is not looked up.
            let Some(parent) = parent else { continue };
            let tag = if variable.param { DW_TAG_formal_parameter } else { DW_TAG_variable };
            let entry = self.named(parent, tag, &variable.name);
            self.dwarf.unit.get_mut(entry).set(DW_AT_type, AttributeValue::UnitRef(ty));
            if let Some(place) = self.place(start, len, &variable.place) {
                self.dwarf.unit.get_mut(entry).set(DW_AT_location, place);
            }
        }
    }

    /// The entry of the scope `scope` of a function, inside the entry of the scope it is in.
    /// `None` is a scope that no code is of, or that is in one, or that is inside itself,
    /// which the scopes of an entry read from the store are not known not to be.
    fn block(
        &mut self,
        function: UnitEntryId,
        start: Address,
        debug: &FunctionDebug,
        blocks: &mut [Option<UnitEntryId>],
        scope: u32,
    ) -> Option<UnitEntryId> {
        // The scopes from this one out that have no entry yet, and the entry they are in.
        let mut without = Vec::new();
        let mut outer = Some(scope);
        let mut parent = function;
        while let Some(scope) = outer {
            if let Some(block) = blocks.get(scope as usize).copied()? {
                parent = block;
                break;
            }
            let Scope { parent, ranges } = debug.scopes.get(scope as usize)?;
            if ranges.is_empty() || without.len() == debug.scopes.len() {
                return None;
            }
            without.push((scope, ranges));
            outer = *parent;
        }
        for (scope, ranges) in without.into_iter().rev() {
            parent = self.lexical_block(parent, start, ranges);
            blocks[scope as usize] = Some(parent);
        }
        Some(parent)
    }

    /// An entry in `parent` for a scope whose code is `ranges` of the function at `start`.
    fn lexical_block(
        &mut self,
        parent: UnitEntryId,
        start: Address,
        ranges: &[(CodeOffset, CodeOffset)],
    ) -> UnitEntryId {
        let block = self.dwarf.unit.add(parent, DW_TAG_lexical_block);
        if let &[(from, to)] = ranges
            && self.apart.is_some()
        {
            self.code(block, past(start, from), (to - from).into());
        } else {
            // One address for the linker to fill in, and the rest counted from it.
            let within = ranges
                .iter()
                .map(|&(from, to)| Range::OffsetPair { begin: from.into(), end: to.into() });
            let ranges =
                std::iter::once(Range::BaseAddress { address: start }).chain(within).collect();
            let ranges = self.dwarf.unit.ranges.add(RangeList(ranges));
            self.dwarf.unit.get_mut(block).set(DW_AT_ranges, AttributeValue::RangeListRef(ranges));
        }
        block
    }

    /// Says of `entry` that its code is the `length` bytes from `begin`. An entry kept apart
    /// names no address, only the place of one among those the object keeps: that is written
    /// as a plain number, which [`split`] says is such a place.
    fn code(&mut self, entry: UnitEntryId, begin: Address, length: u64) {
        let low = match self.apart {
            Some(_) => {
                self.addresses.push(begin);
                AttributeValue::Udata(self.addresses.len() as u64 - 1)
            }
            None => AttributeValue::Address(begin),
        };
        let entry = self.dwarf.unit.get_mut(entry);
        entry.set(DW_AT_low_pc, low);
        entry.set(DW_AT_high_pc, AttributeValue::Udata(length));
    }

    /// Where a variable of a function is, or `None` where it is nowhere to be read.
    fn place(&mut self, start: Address, len: u32, place: &VariablePlace) -> Option<AttributeValue> {
        let ranges = match place {
            VariablePlace::Frame(offset) => {
                let mut place = Expression::new();
                place.op_fbreg(*offset);
                return Some(AttributeValue::Exprloc(place));
            }
            VariablePlace::Ranges(ranges) => ranges,
        };
        if let [(0, to, expression)] = ranges.as_slice()
            && *to >= len
        {
            return Some(AttributeValue::Exprloc(Expression::raw(expression.clone())));
        }
        let places: Vec<Location> = ranges
            .iter()
            .filter(|(from, to, _)| from < to && *from < len)
            .map(|(from, to, expression)| Location::StartLength {
                begin: past(start, *from),
                length: (to.min(&len) - from).into(),
                data: Expression::raw(expression.clone()),
            })
            .collect();
        if places.is_empty() {
            return None;
        }
        let places = self.dwarf.unit.locations.add(LocationList(places));
        Some(AttributeValue::LocationListRef(places))
    }

    /// Describes the function `func` of the module, which is `len` bytes of code.
    fn add_function(&mut self, func: FuncId, len: u32, debug: &FunctionDebug) {
        if len == 0 {
            return;
        }
        if self.ranges.len() == FUNCTIONS_OF_A_UNIT {
            self.end_unit();
        }
        let start = Address::Symbol { symbol: func.as_u32() as usize, addend: 0 };
        let files: Vec<FileId> = debug.files.iter().map(|it| self.file(it)).collect();
        let file = |place: &Place| files.get(place.file as usize).copied();
        self.ranges.push(Range::StartLength { begin: start, length: len.into() });

        debug.linkage_name.hash(&mut self.id);
        len.hash(&mut self.id);
        for (offset, place) in &debug.rows {
            (offset, place.line, place.column).hash(&mut self.id);
        }
        let (outer, name) = last_of(&debug.name);
        let outer = self.namespace(outer);
        let name = self.text(name.as_bytes());
        let linkage_name = self.text(debug.linkage_name.as_bytes());
        let unit = &mut self.dwarf.unit;
        let function = unit.add(outer, DW_TAG_subprogram);
        let entry = unit.get_mut(function);
        entry.set(DW_AT_name, name);
        entry.set(DW_AT_linkage_name, linkage_name);
        self.code(function, start, len.into());
        let entry = self.dwarf.unit.get_mut(function);
        if let Some(declared) = &debug.declared
            && let Some(file) = file(declared)
        {
            entry.set(DW_AT_decl_file, AttributeValue::FileIndex(Some(file)));
            entry.set(DW_AT_decl_line, AttributeValue::Udata(declared.line.into()));
        }
        self.variables(function, start, len, debug);

        // The code ahead of the first statement sets the frame up, which is of the line the
        // function is declared on, or of the first statement when that line is not known.
        let first = debug.rows.first().map(|it| it.1);
        let entry_place = debug.declared.or(first).filter(|_| first.is_some());
        let lead = entry_place.filter(|_| debug.rows.first().is_some_and(|it| it.0 != 0));
        let rows = lead.map(|place| (0, place)).into_iter().chain(debug.rows.iter().copied());
        let program = &mut self.dwarf.unit.line_program;
        let mut any = false;
        for (offset, place) in rows {
            let Some(file) = file(&place) else { continue };
            if !any {
                program.begin_sequence(Some(start));
                any = true;
            }
            let row = program.row();
            row.address_offset = offset.into();
            row.file = file;
            row.line = place.line.into();
            row.column = place.column.into();
            program.generate_row();
        }
        if any {
            program.end_sequence(len.into());
        }
    }

    /// The sections of the units, none of which is kept apart: none when no function is
    /// described.
    ///
    /// Grouped, each unit has its own sections, and only the strings are shared: what a
    /// unit refers to is then its functions and the strings, and an object can be cut into
    /// several with each unit beside its functions (`blaze`'s `link/split.rs`), which the
    /// linker reads on threads of their own.
    fn sections(mut self) -> Result<Vec<Section>, String> {
        self.end_unit();
        let Written { units, mut shared, grouped: mut sections, failed } = self.full;
        if let Some(why) = failed {
            return Err(why);
        }
        if units == 0 {
            return Ok(sections);
        }
        let (strings, line_strings) = (self.dwarf.strings, self.dwarf.line_strings);
        line_strings.write(&mut shared.debug_line_str).map_err(|e| e.to_string())?;
        strings.write(&mut shared.debug_str).map_err(|e| e.to_string())?;
        shared.for_each_mut(|id, section: &mut SectionWriter| {
            sections.push(Section { id, group: None, written: std::mem::take(section) });
            Ok::<_, String>(())
        })?;
        Ok(sections)
    }

    /// Writes the sections into the object, when it has a function to describe.
    /// Answers the files of what is kept apart from it.
    fn emit(mut self, product: &mut ObjectProduct) -> Result<Vec<Sidecar>, String> {
        self.end_unit();
        if !matches!(product.object.format(), BinaryFormat::Elf | BinaryFormat::MachO) {
            return Ok(Vec::new());
        }
        let Some((name, apart)) = self.apart.take() else {
            place(product, self.sections()?)?;
            return Ok(Vec::new());
        };
        let mut split = split::Split::new(self.encoding, self.directory, name, &product.object);
        for unit in apart {
            split.add(unit)?;
        }
        let (sections, files) = split.finish();
        place(product, sections)?;
        Ok(files)
    }
}

/// The units of a [`Dwarf`] that are written.
struct Written {
    units: usize,
    /// The sections every unit shares: all of each unit's but when grouped, and the strings.
    shared: Sections<SectionWriter>,
    /// When grouped, the sections of each unit.
    grouped: Vec<Section>,
    /// Why a unit could not be written, which the sections then say.
    failed: Option<String>,
}

impl Default for Written {
    fn default() -> Self {
        Written {
            units: 0,
            shared: Sections::new(SectionWriter::default()),
            grouped: Vec::new(),
            failed: None,
        }
    }
}

/// One section of what a debugger reads that is linked into the program.
struct Section {
    id: SectionId,
    /// The unit it is of, when each has sections of its own; `None` for those of every unit.
    group: Option<usize>,
    written: SectionWriter,
}

/// A file beside the object with the entries of one unit kept apart from it.
pub struct Sidecar {
    /// Where the unit left in the object says the file is.
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Writes the sections into the object, each with what the linker is to fill in. A Mach-O
/// linker leaves the sections in the object, where a debugger reads them through the debug
/// map: an offset into another section is written as it is, and an address is relocated
/// against its section, which [`settle_macho`] adds the address of once the object is laid out.
fn place(product: &mut ObjectProduct, sections: Vec<Section>) -> Result<(), String> {
    place_all(product, vec![sections])
}

/// [`place`] for the sections of several [`Dwarf`]s, each of which reads only its own.
fn place_all(product: &mut ObjectProduct, all: Vec<Vec<Section>>) -> Result<(), String> {
    let macho = product.object.format() == BinaryFormat::MachO;
    let mut placed = FxHashMap::default();
    let mut linked = Vec::new();
    // A section with nothing in it is still there when another says where in it to read. A
    // unit's own sections are read by that unit, and the shared ones by every unit.
    let read: Vec<(usize, Option<usize>, SectionId)> = (all.iter().enumerate())
        .flat_map(|(of, sections)| sections.iter().map(move |it| (of, it)))
        .flat_map(|(of, it)| it.written.relocs.iter().map(move |reloc| (of, it.group, reloc)))
        .filter_map(|(of, group, it)| match it.target {
            Target::Section(section) => Some((of, group, section)),
            Target::Function(_) => None,
        })
        .collect();
    let sections = (all.into_iter().enumerate())
        .flat_map(|(of, sections)| sections.into_iter().map(move |it| (of, it)));
    for (of, Section { id, group, written }) in sections {
        let is_read = (read.iter())
            .any(|&(at, by, it)| at == of && it == id && (by == group || group.is_none()));
        if written.bytes.slice().is_empty() && !is_read {
            continue;
        }
        let kind = match id {
            SectionId::DebugStr | SectionId::DebugLineStr => SectionKind::DebugString,
            _ => SectionKind::Debug,
        };
        let (segment, name) = if macho {
            let segment = product.object.segment_name(StandardSegment::Debug).to_vec();
            (segment, format!("__{}", &id.name()[1..]).into_bytes())
        } else {
            (Vec::new(), id.name().as_bytes().to_vec())
        };
        let at = product.object.add_section(segment, name, kind);
        product.object.section_mut(at).set_data(written.bytes.slice().to_vec(), 1);
        placed.insert((of, group, id), at);
        linked.push((of, id, group, at, written.relocs));
    }
    for (of, id, group, at, relocs) in linked {
        for reloc in &relocs {
            let (symbol, addend) = match reloc.target {
                Target::Function(func) => function_place(product, func),
                Target::Section(section) => {
                    let found =
                        placed.get(&(of, group, section)).or(placed.get(&(of, None, section)));
                    let Some(&section) = found else {
                        return Err(format!("{} points to a section left out", id.name()));
                    };
                    if macho {
                        let size = usize::from(reloc.size);
                        let field = (product.object.section_mut(at).data_mut())
                            .get_mut(reloc.offset as usize..)
                            .and_then(|it| it.get_mut(..size))
                            .ok_or_else(|| format!("{} has an offset outside it", id.name()))?;
                        field.copy_from_slice(&reloc.addend.to_le_bytes()[..size]);
                        continue;
                    }
                    (product.object.section_symbol(section), 0)
                }
            };
            let flags = RelocationFlags::Generic {
                kind: RelocationKind::Absolute,
                encoding: RelocationEncoding::Generic,
                size: reloc.size * 8,
            };
            let relocation =
                Relocation { offset: reloc.offset, symbol, flags, addend: addend + reloc.addend };
            product.object.add_relocation(at, relocation).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Adds to each address in the debug sections of the Mach-O `object` the address its section
/// was laid out at, as a relocation against a section has it.
pub(super) fn settle_macho(object: &mut [u8]) -> Result<(), String> {
    use object::{Object, ObjectSection, RelocationTarget};
    let file = object::File::parse(&*object).map_err(|e| e.to_string())?;
    let mut settled = Vec::new();
    for section in file.sections() {
        if section.segment_name().ok().flatten() != Some("__DWARF") {
            continue;
        }
        let Some((start, _)) = section.file_range() else { continue };
        for (offset, relocation) in section.relocations() {
            let RelocationTarget::Section(target) = relocation.target() else { continue };
            let address = file.section_by_index(target).map_err(|e| e.to_string())?.address();
            settled.push((start + offset, usize::from(relocation.size() / 8), address));
        }
    }
    drop(file);
    for (at, size, address) in settled {
        let field = object
            .get_mut(at as usize..)
            .and_then(|it| it.get_mut(..size))
            .ok_or("a debug section with a relocation outside it")?;
        let mut held = [0; 8];
        held[..size].copy_from_slice(field);
        let value = u64::from_le_bytes(held).wrapping_add(address);
        field.copy_from_slice(&value.to_le_bytes()[..size]);
    }
    Ok(())
}

/// The path a type named `name` is written at and its name there, as in `a::b` and `C<d::E>`.
/// A name that is not a path, like that of a tuple or of a pointer, is in nothing.
fn within(name: &str) -> (Option<&str>, &str) {
    let generics = name.find('<').unwrap_or(name.len());
    let Some((path, last)) = name[..generics].rsplit_once("::") else { return (None, name) };
    let plain = |it: char| it.is_alphanumeric() || matches!(it, '_' | ':' | '{' | '}' | '#');
    if !path.chars().all(plain) || !last.chars().all(plain) {
        return (None, name);
    }
    (Some(path), &name[path.len() + 2..])
}

/// What a path is in and its last name, as in `a::B<c::D>` and `e<F>`: the last `::` that is
/// in no brackets parts them. A name that is no path is in nothing.
fn last_of(path: &str) -> (Option<&str>, &str) {
    let mut depth = 0u32;
    let mut parted = None;
    let bytes = path.as_bytes();
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            // The arrow of a function's result closes nothing.
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => parted = Some(at),
            _ => {}
        }
    }
    match parted {
        Some(at) if at > 0 && at + 2 < path.len() => (Some(&path[..at]), &path[at + 2..]),
        _ => (None, path),
    }
}

/// The address `by` bytes past `start`.
fn past(start: Address, by: CodeOffset) -> Address {
    match start {
        Address::Constant(start) => Address::Constant(start + u64::from(by)),
        Address::Symbol { symbol, addend } => {
            Address::Symbol { symbol, addend: addend + i64::from(by) }
        }
    }
}

/// The symbol to name a function by and how far past it the function starts. A local
/// function is named through its section, as `.eh_frame` names it.
fn function_place(product: &mut ObjectProduct, func: FuncId) -> (SymbolId, i64) {
    let symbol = product.function_symbol(func);
    match product.object.symbol_section_and_offset(symbol) {
        Some((section, offset)) => (section, offset as i64),
        None => (symbol, 0),
    }
}

/// What a relocation of a debug section points into.
#[derive(Clone, Copy)]
enum Target {
    Function(FuncId),
    Section(SectionId),
}

#[derive(Clone, Copy)]
struct Reloc {
    offset: u64,
    size: u8,
    target: Target,
    addend: i64,
}

/// A [`Writer`] of one debug section that collects the relocations its output needs: the
/// addresses of functions, and the offsets into other debug sections, which a linker moves.
#[derive(Clone)]
struct SectionWriter {
    relocs: Vec<Reloc>,
    bytes: EndianVec<RunTimeEndian>,
}

impl SectionWriter {
    /// Adds what `other` has to the end, with what the linker is to fill in of it.
    fn append(&mut self, other: &SectionWriter) {
        let at = self.bytes.len() as u64;
        self.relocs.extend(other.relocs.iter().map(|it| Reloc { offset: it.offset + at, ..*it }));
        self.bytes.write(other.bytes.slice()).expect("a vector takes all it is given");
    }
}

impl Default for SectionWriter {
    fn default() -> Self {
        SectionWriter { relocs: Vec::new(), bytes: EndianVec::new(RunTimeEndian::Little) }
    }
}

impl Writer for SectionWriter {
    type Endian = RunTimeEndian;

    fn endian(&self) -> Self::Endian {
        self.bytes.endian()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn write(&mut self, bytes: &[u8]) -> WriteResult<()> {
        self.bytes.write(bytes)
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> WriteResult<()> {
        self.bytes.write_at(offset, bytes)
    }

    fn write_address(&mut self, address: Address, size: u8) -> WriteResult<()> {
        match address {
            Address::Constant(val) => self.write_udata(val, size),
            Address::Symbol { symbol, addend } => {
                self.relocs.push(Reloc {
                    offset: self.len() as u64,
                    size,
                    target: Target::Function(FuncId::from_u32(symbol as u32)),
                    addend,
                });
                self.write_udata(0, size)
            }
        }
    }

    fn write_offset(&mut self, val: usize, section: SectionId, size: u8) -> WriteResult<()> {
        self.relocs.push(Reloc {
            offset: self.len() as u64,
            size,
            target: Target::Section(section),
            addend: val as i64,
        });
        self.write_udata(0, size)
    }

    fn write_offset_at(
        &mut self,
        offset: usize,
        val: usize,
        section: SectionId,
        size: u8,
    ) -> WriteResult<()> {
        self.relocs.push(Reloc {
            offset: offset as u64,
            size,
            target: Target::Section(section),
            addend: val as i64,
        });
        self.write_udata_at(offset, 0, size)
    }
}
