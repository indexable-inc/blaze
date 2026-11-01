//! What compiling one function gives: machine code that names the functions and data it uses
//! by what they are, not by where an object file put them. That makes the result a function of
//! the database alone, so it is a query, and a build stitches the results into an object.

#![forbid(unsafe_code)]

use std::{
    cell::RefCell,
    fmt,
    hash::{BuildHasher, Hash, Hasher},
    iter,
    sync::LazyLock,
};

use base_db::{Db, FileId};
use blaze_hash::{FixedHasher, FixedState, HashMap};
use cranelift_codegen::{
    MachReloc, RelocTarget, binemit,
    ir::{self, Signature},
    isa::TargetIsa,
};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_module::{DataId, FuncId, ModuleReloc, ModuleRelocTarget};
use hashbrown::{HashTable, hash_table::Entry};
use hir_def::{
    DefWithBodyId, FunctionId, StaticId, TraitId,
    hir::{BindingId, ExprId},
};

use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    mir::MirSpan,
    next_solver::{DbInterner, GenericArgs, StoredGenericArgs, StoredTy, Ty},
    traits::StoredParamEnvAndCrate,
};

use super::{
    CodegenError, ConstBlob, DataIdentity, Instance, InstanceDef, Result, Target,
    debuginfo::{LineRow, SlotPlaces},
    unwind::FunctionUnwind,
};

/// A function that compiled code calls or takes the address of.
#[derive(Clone, PartialEq, Eq, Hash)] // clone: inside `FuncRequest`, which `referenced` copies as `DataRequest`
pub(super) enum FuncTarget {
    /// A symbol by its link name.
    Symbol(String),
    /// A rustc symbol by its demangled path.
    Path(String),
    /// Whatever stands for the instance in the build: rustc's copy or ours.
    Instance(Instance, StoredParamEnvAndCrate),
    /// The function an `asm!` block is assembled as, by its assembly.
    Asm(String),
}

/// A [`FuncTarget`] by reference: what a request is looked up by before anything of it is
/// cloned, and what [`Wants`] hashes requests by.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum TargetRef<'a> {
    Symbol(&'a str),
    Path(&'a str),
    Instance(&'a Instance, &'a StoredParamEnvAndCrate),
    Asm(&'a str),
}

impl FuncTarget {
    pub(super) fn as_ref(&self) -> TargetRef<'_> {
        match self {
            FuncTarget::Symbol(it) => TargetRef::Symbol(it),
            FuncTarget::Path(it) => TargetRef::Path(it),
            FuncTarget::Instance(instance, env) => TargetRef::Instance(instance, env),
            FuncTarget::Asm(it) => TargetRef::Asm(it),
        }
    }
}

impl TargetRef<'_> {
    /// Made once per request, when it is asked for the first time ([`Wants::func_ref`]).
    fn to_owned(self) -> FuncTarget {
        match self {
            // alloc: the request's own target, once per request
            TargetRef::Symbol(it) => FuncTarget::Symbol(it.to_owned()),
            TargetRef::Path(it) => FuncTarget::Path(it.to_owned()), // alloc: as above
            // clone: as above, refcounts
            TargetRef::Instance(instance, env) => FuncTarget::Instance(instance.clone(), env.clone()),
            // alloc: as above
            TargetRef::Asm(it) => FuncTarget::Asm(it.to_owned()),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct FuncRequest {
    pub(super) target: FuncTarget,
    pub(super) sig: Signature,
}

/// A data object that compiled code or other data points to. The ids inside a blob index the
/// requests of the same [`Requests`].
#[derive(Clone, PartialEq, Eq, Hash)] // clone: `referenced` copies a function's own requests out of its unit's, once per function LLVM compiles
pub(super) enum DataRequest {
    Blob {
        blob: ConstBlob,
        identity: DataIdentity,
    },
    /// One object for the key in the whole build, whichever function asks first.
    Vtable {
        ty: StoredTy,
        trait_ref: Option<(TraitId, StoredGenericArgs)>,
        blob: ConstBlob,
    },
    Static(StaticId),
    /// A `core::panic::Location` for a place in a body. Looked up when stitching, so that code
    /// does not change when lines above it are added.
    Location(Option<(DefWithBodyId, MirSpan)>),
    /// A `core::panic::Location` of where a function is declared: what a pointer to a
    /// `#[track_caller]` function passes it.
    Declaration(FunctionId),
    /// The path of a file as rustc reports it, without a terminator.
    FilePath(FileId),
    /// A `&str` with the paths of files in it.
    Text(Vec<TextPart>),
}

/// A part of a [`DataRequest::Text`].
#[derive(Clone, PartialEq, Eq, Hash)] // clone: inside `DataRequest`
pub(super) enum TextPart {
    Text(String),
    /// The path of a file as rustc reports it.
    FilePath(FileId),
}

/// Everything a compiled function or static refers to. `FuncId` and `DataId` index these
/// until the stitcher replaces them with the ids of its module.
#[derive(Default, PartialEq, Eq, Hash)]
pub(super) struct Requests {
    pub(super) funcs: Vec<FuncRequest>,
    pub(super) datas: Vec<DataRequest>,
}

#[derive(PartialEq)]
pub(super) struct Code {
    pub(super) signature: Signature,
    pub(super) bytes: Vec<u8>,
    pub(super) align: u64,
    /// In the order the code generator made them, which is by offset for Cranelift's.
    pub(super) relocs: Relocs,
    pub(super) unwind: Option<FunctionUnwind>,
    /// Which statement the code is of, from each offset where that changes.
    pub(super) lines: Vec<LineRow>,
    /// Where the slots of named locals are, for a debugger.
    pub(super) slots: SlotPlaces,
}

/// What a relocation of a function's code points at.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum RelocName {
    /// A function the code asks for, by its place among the [`Requests`].
    Func(u32),
    /// A data object the code asks for, by its place among the [`Requests`].
    Data(u32),
    LibCall(ir::LibCall),
    Known(ir::KnownSymbol),
    /// The function itself, this many bytes into its code.
    Own(u32),
    /// A name in a namespace of its own: a body built in ([`INLINED`]) until
    /// [`inline`](super::inline) puts it there, or the body a v2 thunk calls.
    User { namespace: u32, index: u32 },
}

/// One relocation of a function's code, as [`Relocs`] gives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Reloc {
    pub(super) offset: u32,
    pub(super) kind: binemit::Reloc,
    pub(super) name: RelocName,
    pub(super) addend: i64,
    /// The type ELF has for it where Cranelift has no kind for that, and `kind` stands in:
    /// the low bits of an address in an AArch64 load or store, which go in in units of what
    /// it moves, and whatever the assembly of a function names a symbol with. The object is
    /// given the relocation when it is finished.
    pub(super) elf_type: Option<u32>,
}

/// The id a relocation of a function against itself has as a [`ModuleReloc`] until the
/// function is stitched: one below `u32::MAX`, which `FuncId::from_u32` asserts away in
/// debug builds.
pub(super) fn unstitched() -> FuncId {
    FuncId::from_u32(u32::MAX - 1)
}

impl Reloc {
    /// As Cranelift's module takes it, `own` being the function's id there.
    pub(super) fn module(self, own: FuncId) -> ModuleReloc {
        let name = match self.name {
            RelocName::Func(index) => ModuleRelocTarget::user(0, index),
            RelocName::Data(index) => ModuleRelocTarget::user(1, index),
            RelocName::LibCall(call) => ModuleRelocTarget::LibCall(call),
            RelocName::Known(symbol) => ModuleRelocTarget::KnownSymbol(symbol),
            RelocName::Own(offset) => ModuleRelocTarget::FunctionOffset(own, offset),
            RelocName::User { namespace, index } => ModuleRelocTarget::user(namespace, index),
        };
        ModuleReloc { offset: self.offset, kind: self.kind, name, addend: self.addend }
    }
}

impl RelocName {
    /// The name Cranelift's module has for `namespace` and `index`: 0 for functions, 1 for
    /// data, as [`Recorder::declare_func_in_func`] and [`Recorder::declare_data_in_func`]
    /// declare them; any other is [`RelocName::User`].
    pub(super) fn user(namespace: u32, index: u32) -> RelocName {
        match namespace {
            0 => RelocName::Func(index),
            1 => RelocName::Data(index),
            _ => RelocName::User { namespace, index },
        }
    }

    pub(super) fn of_module(
        name: &ModuleRelocTarget,
    ) -> std::result::Result<RelocName, RelocError> {
        Ok(match *name {
            ModuleRelocTarget::User { namespace, index } => RelocName::user(namespace, index),
            ModuleRelocTarget::LibCall(call) => RelocName::LibCall(call),
            ModuleRelocTarget::KnownSymbol(symbol) => RelocName::Known(symbol),
            ModuleRelocTarget::FunctionOffset(_, offset) => RelocName::Own(offset),
        })
    }
}

/// Why a relocation can't be kept as a [`Relocs`] row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RelocError {
    /// An ELF type above 16 bits, which no relocation of x86-64 or AArch64 has.
    ElfType(u32),
    /// An ELF type for an offset where no relocation is, in the order of the relocations.
    Unmatched(u32),
    /// A libcall Cranelift's `LibCall::all_libcalls` does not list.
    LibCall(ir::LibCall),
    /// A name of a Cranelift test case.
    TestCase,
}

impl fmt::Display for RelocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelocError::ElfType(it) => write!(f, "a relocation of ELF type {it}, above 16 bits"),
            RelocError::Unmatched(it) => {
                write!(f, "an ELF type for offset {it}, where no relocation is")
            }
            RelocError::LibCall(it) => write!(f, "a relocation against libcall {it}, unlisted"),
            RelocError::TestCase => f.write_str("a relocation against a test case's name"),
        }
    }
}

impl From<RelocError> for CodegenError {
    fn from(e: RelocError) -> Self {
        // alloc: the error's message
        CodegenError::Backend(e.to_string())
    }
}

/// Which [`RelocName`] a [`Row`] holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum NameTag {
    Func,
    Data,
    /// `index` is the call's place in `LibCall::all_libcalls`.
    LibCall,
    /// `index` is 0 for `ElfGlobalOffsetTable`, 1 for `CoffTlsIndex`.
    Known,
    /// `index` is the offset into the function.
    Own,
    /// `index` is the name's in its namespace, which is in [`Relocs::spaces`].
    User,
}

/// A [`Reloc`] as [`Relocs`] keeps it: 16 bytes, the size data.md budgets a relocation, and
/// read in place (Cranelift's `ModuleReloc` is 32, with its `i64` addend and 12-byte name).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Row {
    offset: u32,
    /// What `name` says the target is by.
    index: u32,
    /// The addend, or [`WIDE`] for one in [`Relocs::wide`].
    addend: i32,
    kind: binemit::Reloc,
    name: NameTag,
    /// The ELF type, 0 for none: `R_*_NONE` is no type a relocation is given.
    elf: u16,
}

const _: () = assert!(size_of::<Row>() == 16);

/// The addend of a [`Row`] whose own is in [`Relocs::wide`]: an addend of `i32::MIN` is
/// kept there too, so that this means one thing.
const WIDE: i32 = i32::MIN;

/// The relocations of a function's code, as columns of 16-byte rows (data.md): offset, target
/// by its `u32` place among the function's requests, kind and a 32-bit addend, with the few
/// addends wider than that, and the namespaces of [`RelocName::User`] names, in side tables.
/// One `Vec` of rows a function, no name owned by a row, and a lookup is an index (a wide
/// addend or a namespace a binary search of the few there are).
#[derive(Default, PartialEq, Eq, Debug)]
pub(super) struct Relocs {
    rows: Vec<Row>,
    /// The addends rows hold [`WIDE`] for, by the row's place, in the order of the rows.
    wide: Vec<(u32, i64)>,
    /// The namespaces of the rows of [`NameTag::User`], by the row's place, in their order.
    spaces: Vec<(u32, u32)>,
}

impl Relocs {
    pub(super) fn with_capacity(capacity: usize) -> Relocs {
        // alloc: empty, allocates nothing until a wide addend or a namespace of its own comes
        Relocs { rows: Vec::with_capacity(capacity), wide: Vec::new(), spaces: Vec::new() }
    }

    /// Cranelift's relocations of `func`'s code after the rows it has, its rows grown once to
    /// hold them: what emit writes into a new list for v1, and into the thread's kept list for
    /// v2 ([`crate::spare::relocs`]).
    pub(super) fn extend_mach(
        &mut self,
        relocs: &[MachReloc],
        func: &ir::Function,
    ) -> std::result::Result<(), RelocError> {
        self.rows.reserve_exact(relocs.len());
        for reloc in relocs {
            let name = match &reloc.target {
                RelocTarget::ExternalName(ir::ExternalName::User(name)) => {
                    let name = &func.params.user_named_funcs()[*name];
                    RelocName::user(name.namespace, name.index)
                }
                RelocTarget::ExternalName(ir::ExternalName::LibCall(call)) => {
                    RelocName::LibCall(*call)
                }
                RelocTarget::ExternalName(ir::ExternalName::KnownSymbol(symbol)) => {
                    RelocName::Known(*symbol)
                }
                RelocTarget::ExternalName(ir::ExternalName::TestCase(_)) => {
                    return Err(RelocError::TestCase);
                }
                RelocTarget::Label(offset) => RelocName::Own(offset.as_offset()),
            };
            let (offset, kind, addend) = (reloc.offset, reloc.kind, reloc.addend);
            self.push(Reloc { offset, kind, name, addend, elf_type: None })?;
        }
        Ok(())
    }

    /// Empties it, keeping its room.
    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.wide.clear();
        self.spaces.clear();
    }

    /// The rows it has room for.
    pub(super) fn capacity(&self) -> usize {
        self.rows.capacity()
    }

    /// `relocs`, with the ELF types of `elf_types` (by offset, in the order of `relocs`) given
    /// to the ones at those offsets.
    pub(super) fn of_module(
        relocs: &[ModuleReloc],
        elf_types: &[(u32, u32)],
    ) -> std::result::Result<Relocs, RelocError> {
        let mut rows = Relocs::with_capacity(relocs.len());
        let mut typed = elf_types.iter().peekable();
        for reloc in relocs {
            let elf_type = typed.next_if(|it| it.0 == reloc.offset).map(|it| it.1);
            rows.push_module(reloc, elf_type)?;
        }
        match typed.next() {
            Some(&(offset, _)) => Err(RelocError::Unmatched(offset)),
            None => Ok(rows),
        }
    }

    pub(super) fn push_module(
        &mut self,
        reloc: &ModuleReloc,
        elf_type: Option<u32>,
    ) -> std::result::Result<(), RelocError> {
        let name = RelocName::of_module(&reloc.name)?;
        let (offset, kind, addend) = (reloc.offset, reloc.kind, reloc.addend);
        self.push(Reloc { offset, kind, name, addend, elf_type })
    }

    pub(super) fn push(&mut self, reloc: Reloc) -> std::result::Result<(), RelocError> {
        let (name, index) = Self::encode(reloc.name)?;
        let elf = match reloc.elf_type {
            None => 0,
            Some(elf) => u16::try_from(elf)
                .ok()
                .filter(|it| *it != 0)
                .ok_or(RelocError::ElfType(elf))?,
        };
        let at = self.rows.len() as u32;
        let addend = match i32::try_from(reloc.addend) {
            Ok(addend) if addend != WIDE => addend,
            _ => {
                self.wide.push((at, reloc.addend));
                WIDE
            }
        };
        if let RelocName::User { namespace, .. } = reloc.name {
            self.spaces.push((at, namespace));
        }
        self.rows.push(Row { offset: reloc.offset, index, addend, kind: reloc.kind, name, elf });
        Ok(())
    }

    fn encode(name: RelocName) -> std::result::Result<(NameTag, u32), RelocError> {
        Ok(match name {
            RelocName::Func(index) => (NameTag::Func, index),
            RelocName::Data(index) => (NameTag::Data, index),
            RelocName::LibCall(call) => {
                let all = ir::LibCall::all_libcalls();
                let at = all.iter().position(|it| *it == call).ok_or(RelocError::LibCall(call))?;
                (NameTag::LibCall, at as u32)
            }
            RelocName::Known(symbol) => (NameTag::Known, match symbol {
                ir::KnownSymbol::ElfGlobalOffsetTable => 0,
                ir::KnownSymbol::CoffTlsIndex => 1,
            }),
            RelocName::Own(offset) => (NameTag::Own, offset),
            RelocName::User { index, .. } => (NameTag::User, index),
        })
    }

    fn name(&self, at: usize, row: &Row) -> RelocName {
        match row.name {
            NameTag::Func => RelocName::Func(row.index),
            NameTag::Data => RelocName::Data(row.index),
            NameTag::LibCall => RelocName::LibCall(ir::LibCall::all_libcalls()[row.index as usize]),
            NameTag::Known => RelocName::Known(match row.index {
                0 => ir::KnownSymbol::ElfGlobalOffsetTable,
                _ => ir::KnownSymbol::CoffTlsIndex,
            }),
            NameTag::Own => RelocName::Own(row.index),
            NameTag::User => {
                let found = self.spaces.binary_search_by_key(&(at as u32), |it| it.0);
                let namespace = self.spaces[found.expect("a namespace is kept for its row")].1;
                RelocName::User { namespace, index: row.index }
            }
        }
    }

    fn decode(&self, at: usize, row: &Row) -> Reloc {
        let addend = match row.addend {
            WIDE => {
                let found = self.wide.binary_search_by_key(&(at as u32), |it| it.0);
                self.wide[found.expect("a wide addend is kept for its row")].1
            }
            addend => i64::from(addend),
        };
        Reloc {
            offset: row.offset,
            kind: row.kind,
            name: self.name(at, row),
            addend,
            elf_type: (row.elf != 0).then_some(u32::from(row.elf)),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(super) fn get(&self, at: usize) -> Reloc {
        self.decode(at, &self.rows[at])
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = Reloc> + Clone + '_ {
        self.rows.iter().enumerate().map(|(at, row)| self.decode(at, row))
    }

    /// Gives each against a function or a data object the number `funcs` or `datas` has for
    /// the one it was against, in place.
    pub(super) fn renumber(
        &mut self,
        mut funcs: impl FnMut(u32) -> u32,
        mut datas: impl FnMut(u32) -> u32,
    ) {
        for row in &mut self.rows {
            match row.name {
                NameTag::Func => row.index = funcs(row.index),
                NameTag::Data => row.index = datas(row.index),
                NameTag::LibCall | NameTag::Known | NameTag::Own | NameTag::User => {}
            }
        }
    }

    /// Moves every one `by` bytes further into the code.
    pub(super) fn shift(&mut self, by: u32) {
        self.rows.iter_mut().for_each(|it| it.offset += by);
    }

    /// The ELF types, as `Code` hashed them when they were a `Vec<(u32, u32)>` of their own:
    /// the hashes of code in the store are unchanged.
    pub(super) fn hash_elf_types<H: Hasher>(&self, state: &mut H) {
        let typed = self.rows.iter().filter(|it| it.elf != 0);
        // clone: of a slice iterator, two pointers; nothing is allocated
        typed.clone().count().hash(state);
        for row in typed {
            (row.offset, u32::from(row.elf)).hash(state);
        }
    }

    /// The relocations, as `Code` hashed them when they were `ModuleReloc`s.
    pub(super) fn hash_as_module<H: Hasher>(&self, state: &mut H) {
        self.rows.len().hash(state);
        for reloc in self.iter() {
            let reloc = reloc.module(unstitched());
            // No kind of relocation holds anything.
            (reloc.offset, std::mem::discriminant(&reloc.kind), reloc.addend).hash(state);
            std::mem::discriminant(&reloc.name).hash(state);
            match &reloc.name {
                ModuleRelocTarget::User { namespace, index } => (namespace, index).hash(state),
                ModuleRelocTarget::LibCall(call) => call.hash(state),
                ModuleRelocTarget::KnownSymbol(symbol) => symbol.hash(state),
                ModuleRelocTarget::FunctionOffset(func, offset) => (func, offset).hash(state),
            }
        }
    }
}

/// Compared by the database when a function is compiled again: one that came out the same
/// leaves what was built from it alone.
#[derive(PartialEq, Hash)]
pub struct CompiledFunction {
    pub(super) requests: Requests,
    pub(super) code: Code,
    /// The body the function was compiled from and the statements of it that
    /// [`Code::lines`] index. Looked up when stitching, like a [`DataRequest::Location`].
    pub(super) source: Option<Source>,
    /// The closure or `async` block of that body the code is of, when it is of one.
    pub(super) closure: Option<ExprId>,
    /// The named locals of the body a debugger can be told of, whose types are of `.0`.
    pub(super) variables: Option<(StoredParamEnvAndCrate, Vec<Variable>)>,
    /// Size and alignment of every type laid out, collected when `BLAZE_LAYOUTS` is set.
    pub(super) layouts: Vec<(String, u64, u64)>,
}

/// The body a function was compiled from, and where each statement its line rows index
/// ([`LineRow::span`]) is written: in the body, or in a body the MIR optimizer inlined into it
/// (`MirBody::inlined`), whose statements keep that body's spans.
#[derive(PartialEq, Hash, Debug)]
pub(super) struct Source {
    pub(super) owner: DefWithBodyId,
    /// Each statement as of the body: one of inlined code is at the call that the outermost
    /// body inlined around it was inlined at, so what reads the body's scopes reads only its
    /// own spans.
    pub(super) spans: Vec<MirSpan>,
    /// The statements of inlined code, by their index in `spans`, in its order.
    pub(super) inlined: Vec<InlinedRow>,
    /// rustc's inlined `SourceScope`s, as `Inlined::scopes` of the body.
    pub(super) scopes: Vec<InlinedScope>,
    /// The body each inlined callee (`Inlined::callees`) is written in, and its closure when
    /// it is one; `None` for one that is no item's, whose code has no place in the source.
    pub(super) callees: Vec<Option<(DefWithBodyId, Option<ExprId>)>>,
}

/// A statement of inlined code: `span` of the body of the callee of `scopes[scope]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct InlinedRow {
    pub(super) at: u32,
    pub(super) scope: u32,
    pub(super) span: MirSpan,
}

/// The code of `callees[callee]` inlined at `call_site` (rustc's `inlined_at`): a span of the
/// body of the callee of `parent`, or of the function's own body without one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct InlinedScope {
    pub(super) callee: u32,
    pub(super) parent: Option<u32>,
    pub(super) call_site: MirSpan,
}

/// Where a span is written: of the body of `owner`, of its closure `closure` when it is one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Written {
    pub(super) owner: DefWithBodyId,
    pub(super) closure: Option<ExprId>,
    pub(super) span: MirSpan,
}

impl Source {
    /// The statements `spans` of `body` were built for, `spans` being as the IR builder
    /// recorded them (`MirSpan::Inlined` for inlined code). A statement of inlined code whose
    /// body or call has no place in the source is an error naming it.
    pub(super) fn of(
        db: &Db,
        owner: DefWithBodyId,
        body: &hir_ty::mir::MirBody<'_>,
        spans: Vec<MirSpan>,
    ) -> std::result::Result<Source, String> {
        let table = &body.inlined;
        if !spans.iter().any(|it| matches!(it, MirSpan::Inlined(_))) {
            // alloc: empty, allocates nothing
            return Source::resolved(owner, &table.spans, &table.scopes, Vec::new(), spans);
        }
        let callees = table
            .callees
            .iter()
            .map(|callee| {
                let closure = match callee.body {
                    hir_ty::mir::InlinedBody::Fn(_) => None,
                    hir_ty::mir::InlinedBody::Closure(closure) => Some(closure.loc(db).expr),
                };
                match hir_ty_mir::mir::owner_of(db, callee.body) {
                    hir_ty::InferBodyId::DefWithBodyId(owner) => Some((owner, closure)),
                    hir_ty::InferBodyId::AnonConstId(_) => None,
                }
            })
            // alloc: the source kept with the compiled function, sized exactly
            .collect();
        Source::resolved(owner, &table.spans, &table.scopes, callees, spans)
            // alloc: the error's message
            .map_err(|what| format!("{what} ({:?})", table.callees))
    }

    /// [`Source::of`] over the tables of `MirBody::inlined`, with where each callee is
    /// written.
    pub(super) fn resolved(
        owner: DefWithBodyId,
        table_spans: &[hir_ty::mir::InlinedSpan],
        table_scopes: &[hir_ty::mir::InlinedScope],
        callees: Vec<Option<(DefWithBodyId, Option<ExprId>)>>,
        mut spans: Vec<MirSpan>,
    ) -> std::result::Result<Source, String> {
        let mut source =
            // alloc: empty, allocates nothing; each is given or filled below
            Source { owner, spans: Vec::new(), inlined: Vec::new(), scopes: Vec::new(), callees };
        if !spans.iter().any(|it| matches!(it, MirSpan::Inlined(_))) {
            source.spans = spans;
            return Ok(source);
        }
        source.scopes = table_scopes
            .iter()
            .map(|scope| {
                let (parent, call_site) = match scope.call_site {
                    MirSpan::Inlined(at) => {
                        let at = table_spans[at as usize];
                        (Some(at.scope), at.span)
                    }
                    span => (None, span),
                };
                InlinedScope { callee: scope.callee, parent, call_site }
            })
            // alloc: the source kept with the compiled function, sized exactly
            .collect();
        for (at, span) in spans.iter_mut().enumerate() {
            let MirSpan::Inlined(n) = *span else { continue };
            let inlined = table_spans[n as usize];
            let row = InlinedRow { at: at as u32, scope: inlined.scope, span: inlined.span };
            source.inlined.push(row);
            let mut scope = inlined.scope;
            // Each scope's parent was made before it, so the chain ends within as many steps.
            for _ in 0..=source.scopes.len() {
                let InlinedScope { callee, parent, call_site } = source.scopes[scope as usize];
                if source.callees.get(callee as usize).copied().flatten().is_none() {
                    // alloc: the error's message
                    return Err(format!(
                        "statement {at} is of inlined callee {callee}, inlined at \
                         {call_site:?}, which is of no item's body: its lines have no place in \
                         the source"
                    ));
                }
                match parent {
                    Some(parent) => scope = parent,
                    None => {
                        *span = call_site;
                        break;
                    }
                }
            }
            if matches!(span, MirSpan::Inlined(_)) {
                // alloc: the error's message
                return Err(format!("statement {at} is of an inlined scope that is its own parent"));
            }
        }
        source.spans = spans;
        Ok(source)
    }

    /// Where statement `at` is written; `closure` is the function's own closure, when it is
    /// one. `None` for no statement.
    pub(super) fn written(&self, at: u32, closure: Option<ExprId>) -> Option<Written> {
        if let Ok(found) = self.inlined.binary_search_by_key(&at, |it| it.at) {
            let row = self.inlined[found];
            return self.of_scope(row.scope, row.span);
        }
        Some(Written { owner: self.owner, closure, span: *self.spans.get(at as usize)? })
    }

    /// Where `scope` is inlined at: a span of its parent's callee, or of the function's own
    /// body (of its closure `closure`). `None` where the parent's callee is no item's, which
    /// [`Source::of`] refused for every scope a statement is of.
    pub(super) fn call_site(&self, scope: u32, closure: Option<ExprId>) -> Option<Written> {
        let InlinedScope { parent, call_site, .. } = *self.scopes.get(scope as usize)?;
        match parent {
            Some(parent) => self.of_scope(parent, call_site),
            None => Some(Written { owner: self.owner, closure, span: call_site }),
        }
    }

    /// The innermost inlined scope statement `at` is of, `None` for the body's own code.
    pub(super) fn scope_of(&self, at: u32) -> Option<u32> {
        let found = self.inlined.binary_search_by_key(&at, |it| it.at).ok()?;
        Some(self.inlined[found].scope)
    }

    /// `span` of the body of the callee of `scope`.
    fn of_scope(&self, scope: u32, span: MirSpan) -> Option<Written> {
        let callee = self.scopes.get(scope as usize)?.callee;
        let (owner, closure) = (*self.callees.get(callee as usize)?)?;
        Some(Written { owner, closure, span })
    }
}

/// A local written with a name that is kept in one slot of the frame for as long as the
/// function runs.
#[derive(PartialEq, Hash)]
pub(super) struct Variable {
    pub(super) binding: BindingId,
    pub(super) param: bool,
    pub(super) ty: StoredTy,
    /// The number of the stack slot it is kept in.
    pub(super) slot: u32,
    /// The slot holds an address rather than the local, which is this far from it.
    pub(super) pointer: Option<u32>,
}

/// Of everything `eq` compares.
impl Hash for Code {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash_with(state, true);
    }
}

impl Code {
    /// Its [`Hash`], leaving out where the named locals are unless `slots`.
    pub(super) fn hash_with<H: Hasher>(&self, state: &mut H, slots: bool) {
        let Code { signature, bytes, align, relocs, unwind, lines, slots: places } = self;
        (signature, bytes, align).hash(state);
        relocs.hash_elf_types(state);
        lines.hash(state);
        if slots {
            places.hash(state);
        }
        // Cranelift's unwind rules have no `Hash`: what they print is all of them, hashed as
        // the `String` of it was.
        PRINTED.with_borrow_mut(|printed| {
            printed.clear();
            fmt::Write::write_fmt(printed, format_args!("{unwind:?}"))
                .expect("a Debug implementation returned an error");
            printed.as_str().hash(state);
        });
        relocs.hash_as_module(state);
    }
}

thread_local! {
    /// What [`Code::hash_with`] prints unwind rules into, kept for the thread's next function.
    // alloc: empty, allocates nothing until the first print, then grows to the longest
    static PRINTED: RefCell<String> = const { RefCell::new(String::new()) };
}

/// A local written with a name, as the function that is being built keeps it.
pub(super) struct Named {
    pub(super) binding: BindingId,
    pub(super) param: bool,
    pub(super) ty: StoredTy,
    pub(super) slot: ir::StackSlot,
    /// The slot holds an address rather than the local, which is this far from it.
    pub(super) pointer: Option<u32>,
}

/// What the Rust type of a parameter that is an address says of the memory there, which LLVM
/// is told as rustc tells it: `nonnull`, `align`, `dereferenceable`, `noalias`, `readonly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Pointee {
    /// Nothing reaches the memory but through this address while the function runs: a
    /// `&mut T` of a `T` that is `Unpin`, or a `&T` of one without an `UnsafeCell`.
    pub(super) unique: bool,
    /// Nothing writes the memory while the function runs: a `&T` without an `UnsafeCell`.
    pub(super) frozen: bool,
    /// How many bytes from the address may be read at any time while the function runs.
    pub(super) size: u64,
    pub(super) align: u64,
}

/// The value of a static rustc exports no symbol for.
#[derive(PartialEq, Hash)]
pub struct CompiledStatic {
    pub(super) requests: Requests,
    pub(super) blob: ConstBlob,
    pub(super) writable: bool,
}

impl CompiledStatic {
    /// The size and alignment of its value.
    pub(super) fn layout(&self) -> (u64, u64) {
        (self.blob.bytes.len() as u64, self.blob.align)
    }
}

/// How many requests a [`Recorder`] had at some point, to tell the ones made since.
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "llvm"), expect(dead_code))]
pub(super) struct Mark {
    pub(super) funcs: usize,
    pub(super) datas: usize,
}

/// A function built to be put where it is called, and what it refers to.
pub(super) struct Inlinable {
    pub(super) func: ir::Function,
    pub(super) requests: Requests,
    /// What its parameters point to, as [`Recorder::pointees`].
    pub(super) pointees: Vec<(u32, Pointee)>,
    /// The bodies of what it calls that LLVM is shown, as [`Recorder::seen`].
    pub(super) seen: Vec<Seen>,
    /// The instances whose bodies it has in it, as [`Recorder::built_in`].
    pub(super) built_in: Vec<Instance>,
}

impl Inlinable {
    /// `func` with `requests`, if a caller can ask for the same: each blob points only at
    /// what was asked for before it, and none has a `TypeId` to hash. A hash that can't be
    /// had fails the function that has it, whose callers then call rustc's copy of it.
    pub(super) fn of(
        func: ir::Function,
        requests: Requests,
        pointees: Vec<(u32, Pointee)>,
        seen: Vec<Seen>,
        built_in: Vec<Instance>,
    ) -> Option<Inlinable> {
        let hashed_later = requests.datas.iter().any(|it| match it {
            DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => {
                !blob.type_ids.is_empty()
            }
            _ => false,
        });
        (requests.ordered() && !hashed_later).then_some(Inlinable {
            func,
            requests,
            pointees,
            seen,
            built_in,
        })
    }
}

impl Requests {
    /// Whether each blob points only at what was asked for before it, as
    /// [`Recorder::moved_requests`] takes them.
    pub(super) fn ordered(&self) -> bool {
        self.datas.iter().enumerate().all(|(at, request)| match request {
            DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => {
                blob.relocs.iter().all(|it| (it.1.as_u32() as usize) < at)
            }
            _ => true,
        })
    }
}

/// The body of a function the function being built calls, which LLVM is shown in a compile
/// at a level above 1 (see `tier::seen_body`): it builds the body in where it finds that
/// worth it, and knows what a call it keeps does.
pub(super) struct Seen {
    /// The function the calls are of, among the requests.
    pub(super) id: FuncId,
    pub(super) func: ir::Function,
    pub(super) pointees: Vec<(u32, Pointee)>,
    pub(super) hint: Hint,
}

/// What a function's `#[inline]` asks of LLVM where its body is shown, as rustc asks it:
/// `inlinehint`, `alwaysinline` or `noinline`. The summary an importer reads says the same.
pub(super) use blaze_codegen::summary::Hint;

/// The namespace of the names of callees that are built into their callers. No relocation
/// has it, as no call of one is left when the caller is compiled.
pub(super) const INLINED: u32 = u32::MAX;

/// Collects the requests of one function while its IR is built.
pub(super) struct Recorder<'db> {
    pub(super) db: &'db Db,
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    pub(super) isa: &'static dyn TargetIsa,
    /// What `isa` generates code for.
    pub(super) target: &'static Target,
    /// Whether the crate compiling the function has `-C overflow-checks`.
    pub(super) checked: bool,
    /// The settings of the v2 unit the function is built for ([`super::v2::build::building`]):
    /// its `-C debug-assertions` and panic strategy in place of the crate's, and no callee's
    /// body built in, which the unit's key would not name. `None` outside a v2 unit.
    pub(super) unit: Option<super::v2::build::Build>,
    /// What it asks for, which is all a producer of its code needs of this.
    pub(super) wants: Wants,
    /// The places in `wants.requests.datas` of the vtables, by the hash of their key
    /// ([`vtable_hash`]): looked up without cloning a key, whose types clone by refcount.
    vtables: HashTable<Slot>,
    statics: HashMap<StaticId, DataId>,
    pub(super) layouts: Option<RefCell<HashMap<Ty<'db>, (u64, u64)>>>,
    /// With `layouts`: each `async fn`'s future as rustc names it, with the offsets of what
    /// each of its variants holds, for `harness/layoutdiff.py` to set against rustc's.
    pub(super) coroutine_layouts: RefCell<Vec<(String, u64, u64)>>,
    pub(super) builder_ctx: FunctionBuilderContext,
    /// The assembly of the function, when it is a `#[naked]` one, and what each operand of
    /// it that is a `sym` names.
    pub(super) naked: Option<(String, Vec<Option<ModuleRelocTarget>>)>,
    /// The statements instructions were built for, which their source locations index.
    spans: Vec<MirSpan>,
    span_ids: HashMap<MirSpan, u32>,
    /// The locals written with a name that a stack slot of their own tells where they are.
    pub(super) named: Vec<Named>,
    /// The slots of the locals of the body, for [`overlap`](super::overlap) to share.
    pub(super) frame_locals: Option<super::overlap::FrameLocals>,
    /// The bodies of the callees that are built into their callers, which call them by their
    /// place here in [`INLINED`] until [`inline`](super::inline) puts the bodies there.
    pub(super) inlined: Vec<ir::Function>,
    /// The instances whose bodies are in [`Self::inlined`], and the ones those have in them,
    /// each once: a change to the code of one is a change to this function's.
    pub(super) built_in: Vec<Instance>,
    /// The places in [`Self::built_in`], by the hash of the instance there.
    built_in_at: HashTable<Slot>,
    /// The bodies of [`Self::inlined`] that a second call of the same callee is given again:
    /// one clone and one renaming of a callee's body a function, not one a call.
    reused: HashTable<Reused>,
    /// The function is built to be put in its callers, whose statements its instructions
    /// are then of: it marks none with a statement of its own.
    pub(super) unlocated: bool,
    /// The function hides a value from the optimizer, so it is called and not built into
    /// its callers, where the value would be in plain sight.
    pub(super) opaque: bool,
}

/// What a function asks for, as its IR is built and as a producer of its code adds to it:
/// the part of a [`Recorder`] that has nothing of the database, which LLVM is run with
/// once the database is let go of (see `tier::Prepared`).
pub(super) struct Wants {
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    pub(super) isa: &'static dyn TargetIsa,
    pub(super) requests: Requests,
    /// The places in `requests.funcs`, by the hash of the request there ([`func_hash`]): 16
    /// bytes a request, the request itself kept once, and one hash and one probe an ask.
    funcs: HashTable<Slot>,
    /// The places in `requests.datas` of the [`DataIdentity::Shared`] blobs, by the hash of
    /// the blob ([`BlobView::hash`]).
    shared: HashTable<Slot>,
    /// The parameters of the function that are addresses of memory its signature says
    /// something of, by their place among the parameters of its IR.
    pub(super) pointees: Vec<(u32, Pointee)>,
    /// The bodies of functions it calls that LLVM is shown, in its names.
    pub(super) seen: Vec<Seen>,
}

/// An entry of a table of places: the hash is kept so that growing the table hashes nothing
/// again.
#[derive(Clone, Copy)]
struct Slot {
    hash: u64,
    at: u32,
}

/// A callee's body as [`Recorder::declare_inlined`] put it among [`Recorder::inlined`], by
/// the instance alone: a body is built of the instance and nothing of the call (`tier::body`
/// keys it by the instance and `track_caller`, which is the function's), and its data
/// (locations, texts, blobs of its own) is one object per body, as rustc has one per function.
/// Not by the body's address, which a rebuilt body may or may not get again.
struct Reused {
    hash: u64,
    instance: Instance,
    at: u32,
}

fn vtable_hash(ty: &StoredTy, trait_ref: &Option<(TraitId, StoredGenericArgs)>) -> u64 {
    FixedState.hash_one((ty, trait_ref))
}

fn func_hash(target: TargetRef<'_>, sig: &Signature) -> u64 {
    let mut hasher = FixedHasher::default();
    target.hash(&mut hasher);
    sig.hash(&mut hasher);
    hasher.finish()
}

/// A blob as it is, or as it is once its relocations are renamed ([`Recorder::moved_requests`]):
/// what [`BlobView::hash`] hashes and [`BlobView::is`] compares, without making the blob.
struct BlobView<'a> {
    blob: &'a ConstBlob,
    datas: Option<&'a [DataId]>,
    funcs: Option<&'a [FuncId]>,
}

impl BlobView<'_> {
    fn data(&self, id: DataId) -> u32 {
        self.datas.map_or(id, |it| it[id.as_u32() as usize]).as_u32()
    }

    fn func(&self, id: FuncId) -> u32 {
        self.funcs.map_or(id, |it| it[id.as_u32() as usize]).as_u32()
    }

    fn hash(&self) -> u64 {
        let blob = self.blob;
        let mut hasher = FixedHasher::default();
        (&blob.bytes, blob.align, &blob.type_ids).hash(&mut hasher);
        blob.relocs.len().hash(&mut hasher);
        for &(at, to, addend) in &blob.relocs {
            (at, self.data(to), addend).hash(&mut hasher);
        }
        blob.fn_relocs.len().hash(&mut hasher);
        for &(at, to) in &blob.fn_relocs {
            (at, self.func(to)).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Whether `other`, a blob as it is, is this one.
    fn is(&self, other: &ConstBlob) -> bool {
        let blob = self.blob;
        blob.bytes == other.bytes
            && blob.align == other.align
            && blob.type_ids == other.type_ids
            && blob.relocs.len() == other.relocs.len()
            && blob.fn_relocs.len() == other.fn_relocs.len()
            && iter::zip(&blob.relocs, &other.relocs).all(|(&(at, to, addend), theirs)| {
                (at, self.data(to), addend) == (theirs.0, theirs.1.as_u32(), theirs.2)
            })
            && iter::zip(&blob.fn_relocs, &other.fn_relocs)
                .all(|(&(at, to), theirs)| (at, self.func(to)) == (theirs.0, theirs.1.as_u32()))
    }

    /// The blob, renamed. Made once per blob, when it was not asked for before
    /// ([`Wants::define_view_as`]), or for a vtable or a blob that is not shared.
    fn made(&self) -> ConstBlob {
        let blob = self.blob;
        if self.datas.is_none() && self.funcs.is_none() {
            // clone: the request's own blob, made once as above
            return blob.clone();
        }
        ConstBlob {
            // clone: as above
            bytes: blob.bytes.clone(),
            relocs: blob
                .relocs
                .iter()
                .map(|&(at, to, addend)| (at, DataId::from_u32(self.data(to)), addend))
                // alloc: as above, sized exactly
                .collect(),
            fn_relocs: blob
                .fn_relocs
                .iter()
                .map(|&(at, to)| (at, FuncId::from_u32(self.func(to))))
                // alloc: as above, sized exactly
                .collect(),
            align: blob.align,
            // clone: as above
            type_ids: blob.type_ids.clone(),
        }
    }
}

impl Wants {
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    pub(super) fn new(isa: &'static dyn TargetIsa) -> Wants {
        Wants {
            isa,
            requests: Requests::default(),
            funcs: HashTable::new(),
            shared: HashTable::new(),
            // alloc: empty, allocates nothing
            pointees: Vec::new(),
            seen: Vec::new(), // alloc: empty, allocates nothing
        }
    }

    pub(super) fn into_requests(self) -> Requests {
        self.requests
    }

    /// The bytes of each data object asked for so far that is read-only and points nowhere,
    /// by its place among the requests: what a load from it is known to give.
    #[cfg(feature = "llvm")]
    pub(super) fn constants(&self) -> impl Iterator<Item = (u32, &[u8])> + '_ {
        self.readonly()
            .filter(|it| it.1.points_nowhere())
            .map(|(index, blob)| (index, blob.bytes()))
    }

    /// Each data object asked for so far that is read-only, by its place among the requests.
    /// One with a `TypeId` is left out: its bytes are const eval's ids until the object's data
    /// has the hashes written in their place.
    #[cfg(feature = "llvm")]
    pub(super) fn readonly(&self) -> impl Iterator<Item = (u32, &ConstBlob)> + '_ {
        self.requests.datas.iter().enumerate().filter_map(|(index, data)| match data {
            DataRequest::Blob { blob, identity }
                if *identity != DataIdentity::Writable && blob.type_ids.is_empty() =>
            {
                Some((index as u32, blob))
            }
            _ => None,
        })
    }

    /// The places among the requests of the data objects that are statics. Two of them can be
    /// one object: an `extern` static and the `#[no_mangle]` one it names.
    #[cfg(feature = "llvm")]
    pub(super) fn statics(&self) -> impl Iterator<Item = u32> + '_ {
        let statics = self.requests.datas.iter().enumerate();
        statics.filter_map(|(index, data)| {
            matches!(data, DataRequest::Static(_)).then_some(index as u32)
        })
    }

    /// The number of the function asked for that goes on unwinding, `_Unwind_Resume`.
    #[cfg(feature = "llvm")]
    pub(super) fn resume(&self) -> Option<u32> {
        self.symbol("_Unwind_Resume")
    }

    /// The number of the function asked for that compares memory, `memcmp`.
    #[cfg(feature = "llvm")]
    pub(super) fn memcmp(&self) -> Option<u32> {
        self.symbol("memcmp")
    }

    /// The `asm!` blocks it calls the wrappers of that go where they are called as LLVM's
    /// inline assembly, by the places of the wrappers among the requests.
    #[cfg(feature = "llvm")]
    pub(super) fn asm_blocks(&self) -> HashMap<u32, &'static super::asm::Inline> {
        let funcs = self.requests.funcs.iter().enumerate();
        funcs
            .filter_map(|(at, it)| match &it.target {
                FuncTarget::Asm(text) => Some((at as u32, super::asm::inline_block(text)?)),
                _ => None,
            })
            // Empty, allocating nothing, unless the function calls an `asm!` wrapper.
            // alloc: once per function LLVM compiles; `llvm.rs` takes the map
            .collect()
    }

    /// The texts of the `asm!` wrappers it asks for, by their places among the requests:
    /// what LLVM puts in place of a call of one ([`Self::asm_blocks`]).
    #[cfg(feature = "llvm")]
    pub(super) fn asm_texts(&self) -> impl Iterator<Item = (u32, &str)> {
        let funcs = self.requests.funcs.iter().enumerate();
        funcs.filter_map(|(at, it)| match &it.target {
            FuncTarget::Asm(text) => Some((at as u32, text.as_str())),
            _ => None,
        })
    }

    #[cfg(feature = "llvm")]
    fn symbol(&self, symbol: &str) -> Option<u32> {
        let mut funcs = self.requests.funcs.iter();
        let found =
            funcs.position(|it| matches!(&it.target, FuncTarget::Symbol(name) if name == symbol));
        found.map(|it| it as u32)
    }

    #[cfg(feature = "llvm")]
    pub(super) fn mark(&self) -> Mark {
        Mark { funcs: self.requests.funcs.len(), datas: self.requests.datas.len() }
    }

    /// The requests made since `mark`.
    #[cfg(feature = "llvm")]
    pub(super) fn since(&self, mark: Mark) -> (&[FuncRequest], &[DataRequest]) {
        (&self.requests.funcs[mark.funcs..], &self.requests.datas[mark.datas..])
    }

    /// Which of the requests `code` refers to, directly or through the data it refers to,
    /// with `code` renumbered to them. `None`, and `code` as it was, when it refers to all.
    #[cfg(feature = "llvm")]
    fn kept(&self, code: &mut Code) -> Option<Kept> {
        let requests = &self.requests;
        // alloc: once per function LLVM compiles, beside which it is nothing
        let mut funcs = vec![false; requests.funcs.len()];
        let mut datas = vec![false; requests.datas.len()]; // alloc: as above
        // alloc: as above
        let mut reached = Vec::new();
        for reloc in code.relocs.iter() {
            match reloc.name {
                RelocName::Func(index) => funcs[index as usize] = true,
                RelocName::Data(index) => reached.push(index as usize),
                _ => {}
            }
        }
        while let Some(at) = reached.pop() {
            if std::mem::replace(&mut datas[at], true) {
                continue;
            }
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } =
                &requests.datas[at]
            {
                reached.extend(blob.relocs.iter().map(|it| it.1.as_u32() as usize));
                for &(_, func) in &blob.fn_relocs {
                    funcs[func.as_u32() as usize] = true;
                }
            }
        }
        if funcs.iter().chain(&datas).all(|&it| it) {
            return None;
        }
        let numbered = |kept: &[bool]| {
            let mut next = 0;
            let numbers = kept.iter().map(|&it| {
                let number = next;
                next += u32::from(it);
                it.then_some(number)
            });
            // alloc: as above, sized exactly, kept in `Kept`
            numbers.collect::<Vec<Option<u32>>>()
        };
        let kept = Kept { funcs: numbered(&funcs), datas: numbered(&datas) };
        code.relocs.renumber(
            |index| kept.funcs[index as usize].unwrap(),
            |index| kept.datas[index as usize].unwrap(),
        );
        Some(kept)
    }

    /// Of what the functions of a unit ask for, what `code`, one function's, refers to, in
    /// its own numbers, which `code` is renumbered to.
    #[cfg(feature = "llvm")]
    pub(super) fn referenced(&self, code: &mut Code) -> Requests {
        // The function's own requests out of the unit's, which stay the unit's: once per
        // function of a unit LLVM compiles.
        let Some(kept) = self.kept(code) else {
            let Requests { funcs, datas } = &self.requests;
            // clone: the function's own requests, as above
            return Requests { funcs: funcs.clone(), datas: datas.clone() };
        };
        let funcs = iter::zip(&self.requests.funcs, &kept.funcs);
        // clone: as above
        let funcs = funcs.filter(|it| it.1.is_some()).map(|it| it.0.clone()).collect();
        let datas = iter::zip(&self.requests.datas, &kept.datas).filter(|it| it.1.is_some());
        let datas = datas.map(|(request, _)| {
            let mut request = request.clone(); // clone: as above
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } = &mut request
            {
                kept.renumber(blob);
            }
            request
        });
        // alloc: as above
        Requests { funcs, datas: datas.collect() }
    }

    pub(super) fn func(&mut self, target: FuncTarget, sig: &Signature) -> FuncId {
        let hash = func_hash(target.as_ref(), sig);
        self.func_with(
            hash,
            target,
            |target, it| it.target == *target && it.sig == *sig,
            // clone: once per request, when it is asked for the first time
            |target| FuncRequest { target, sig: sig.clone() },
        )
    }

    /// [`Self::func`] of a target that is cloned only when it was not asked for before.
    fn func_ref(&mut self, target: TargetRef<'_>, sig: &Signature) -> FuncId {
        self.func_with(
            func_hash(target, sig),
            target,
            |target, it| it.target.as_ref() == *target && it.sig == *sig,
            // clone: once per request, when it is asked for the first time
            |target| FuncRequest { target: target.to_owned(), sig: sig.clone() },
        )
    }

    /// The request `same` says `key` is, hashed to `hash` ([`func_hash`]): the one asked for
    /// before, or a new one `make` makes of `key`, which is all that is cloned.
    fn func_with<K>(
        &mut self,
        hash: u64,
        key: K,
        same: impl Fn(&K, &FuncRequest) -> bool,
        make: impl FnOnce(K) -> FuncRequest,
    ) -> FuncId {
        let funcs = &mut self.requests.funcs;
        let entry = self.funcs.entry(
            hash,
            |it| it.hash == hash && same(&key, &funcs[it.at as usize]),
            |it| it.hash,
        );
        match entry {
            Entry::Occupied(it) => FuncId::from_u32(it.get().at),
            Entry::Vacant(it) => {
                let at = funcs.len() as u32;
                it.insert(Slot { hash, at });
                funcs.push(make(key));
                FuncId::from_u32(at)
            }
        }
    }

    fn data(&mut self, request: DataRequest) -> DataId {
        let id = DataId::from_u32(self.requests.datas.len() as u32);
        self.requests.datas.push(request);
        id
    }

    pub(super) fn import(&mut self, symbol: &str, sig: &Signature) -> Result<FuncId> {
        Ok(self.func_ref(TargetRef::Symbol(symbol), sig))
    }

    pub(super) fn import_path(&mut self, path: &str, sig: &Signature) -> FuncId {
        self.func_ref(TargetRef::Path(path), sig)
    }

    /// A function of assembly. The text is assembled here, on the thread compiling the caller,
    /// into the cache [`super::asm::assemble`] keeps: the one thread that puts functions in the
    /// object would otherwise run the assembler for each block in turn. An error is not kept,
    /// so that thread meets it again and refuses the caller with it as before.
    pub(super) fn assembly(&mut self, text: String, sig: &Signature) -> FuncId {
        // alloc: once per `asm!` block, which is then assembled; `asm::assemble` takes text
        _ = super::asm::assemble(&self.isa.triple().to_string(), &text);
        self.func(FuncTarget::Asm(text), sig)
    }

    pub(super) fn define_const(&mut self, blob: ConstBlob) -> Result<DataId> {
        self.define_data(blob, false)
    }

    pub(super) fn define_data(&mut self, blob: ConstBlob, writable: bool) -> Result<DataId> {
        let identity = if writable { DataIdentity::Writable } else { DataIdentity::Shared };
        Ok(self.define_data_as(blob, identity))
    }

    pub(super) fn define_data_as(&mut self, blob: ConstBlob, identity: DataIdentity) -> DataId {
        if identity != DataIdentity::Shared {
            return self.data(DataRequest::Blob { blob, identity });
        }
        let hash = BlobView { blob: &blob, datas: None, funcs: None }.hash();
        self.shared_with(
            hash,
            blob,
            |blob, it| BlobView { blob, datas: None, funcs: None }.is(it),
            |blob| blob,
        )
    }

    /// [`Self::define_data_as`] of `view` as it is renamed, made only when it was not
    /// asked for before.
    fn define_view_as(&mut self, view: BlobView<'_>, identity: DataIdentity) -> DataId {
        if identity != DataIdentity::Shared {
            return self.data(DataRequest::Blob { blob: view.made(), identity });
        }
        self.shared_with(view.hash(), view, |view, it| view.is(it), |view| view.made())
    }

    /// The shared blob `same` says `key` is, hashed to `hash` ([`BlobView::hash`]): the one
    /// asked for before, or a new one `make` makes of `key`.
    fn shared_with<K>(
        &mut self,
        hash: u64,
        key: K,
        same: impl Fn(&K, &ConstBlob) -> bool,
        make: impl FnOnce(K) -> ConstBlob,
    ) -> DataId {
        let datas = &mut self.requests.datas;
        let entry = self.shared.entry(
            hash,
            |it| {
                it.hash == hash
                    && matches!(&datas[it.at as usize], DataRequest::Blob { blob, .. } if same(&key, blob))
            },
            |it| it.hash,
        );
        match entry {
            Entry::Occupied(it) => DataId::from_u32(it.get().at),
            Entry::Vacant(it) => {
                let at = datas.len() as u32;
                it.insert(Slot { hash, at });
                datas.push(DataRequest::Blob { blob: make(key), identity: DataIdentity::Shared });
                DataId::from_u32(at)
            }
        }
    }

    pub(super) fn make_signature(&self) -> Signature {
        Signature::new(self.isa.default_call_conv())
    }

    /// Whether what another object defines is reached as what this one does: the linker of
    /// a PE image puts a stub where an import is called, and there is no other way.
    fn within_image(&self) -> bool {
        self.isa.triple().binary_format == target_lexicon::BinaryFormat::Coff
    }

    /// Names a requested function in `func`, to call it. Whether the function ends up in
    /// this object is not known here: the call names its callee in place, as rustc's do,
    /// and the linker (or the splice) sends it through a stub of its own where the callee
    /// is another library's or out of a branch's reach.
    pub(super) fn declare_func_in_func(&self, id: FuncId, func: &mut ir::Function) -> ir::FuncRef {
        self.declare_func(id, func, true)
    }

    /// Names a requested function in `func`, to take its address: it is reached the way a
    /// function of another object is, which a call need not be.
    pub(super) fn declare_func_address_in_func(
        &self,
        id: FuncId,
        func: &mut ir::Function,
    ) -> ir::FuncRef {
        self.declare_func(id, func, self.within_image())
    }

    fn declare_func(&self, id: FuncId, func: &mut ir::Function, colocated: bool) -> ir::FuncRef {
        let signature =
            // clone: Cranelift's `import_signature` takes it by value into the function's own table
            func.import_signature(self.requests.funcs[id.as_u32() as usize].sig.clone());
        let name = func.declare_imported_user_function(ir::UserExternalName {
            namespace: 0,
            index: id.as_u32(),
        });
        func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::user(name),
            signature,
            colocated,
            patchable: false,
        })
    }
}

impl<'db> std::ops::Deref for Recorder<'db> {
    type Target = Wants;

    fn deref(&self) -> &Wants {
        &self.wants
    }
}

impl<'db> std::ops::DerefMut for Recorder<'db> {
    fn deref_mut(&mut self) -> &mut Wants {
        &mut self.wants
    }
}

/// `BLAZE_LAYOUTS`, read once: the environment's lock is not taken for each function.
static LAYOUTS: LazyLock<bool> = LazyLock::new(|| std::env::var_os("BLAZE_LAYOUTS").is_some());

impl<'db> Recorder<'db> {
    /// The stack slots that hold a local written with a name.
    pub(super) fn named_slots(&self) -> impl Iterator<Item = ir::StackSlot> + '_ {
        self.named.iter().map(|it| it.slot)
    }

    pub(super) fn new(
        db: &'db Db,
        // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
        isa: &'static dyn TargetIsa,
        target: &'static Target,
        checked: bool,
    ) -> Self {
        Recorder {
            db,
            isa,
            target,
            checked,
            unit: super::v2::build::building(),
            wants: Wants::new(isa),
            vtables: HashTable::new(),
            statics: HashMap::default(),
            layouts: (*LAYOUTS).then(RefCell::default),
            coroutine_layouts: RefCell::default(),
            builder_ctx: FunctionBuilderContext::new(),
            naked: None,
            // alloc: empty, allocates nothing, as are the others
            spans: Vec::new(),
            span_ids: HashMap::default(),
            named: Vec::new(), // alloc: empty, allocates nothing
            frame_locals: None,
            // alloc: empty, allocates nothing
            inlined: Vec::new(),
            built_in: Vec::new(), // alloc: empty, allocates nothing
            built_in_at: HashTable::new(),
            reused: HashTable::new(),
            unlocated: false,
            opaque: false,
        }
    }

    /// What marks an instruction as built for the statement at `span`.
    pub(super) fn source_loc(&mut self, span: MirSpan) -> ir::SourceLoc {
        if span == MirSpan::Unknown || self.unlocated {
            return ir::SourceLoc::default();
        }
        let next = self.spans.len() as u32;
        let id = *self.span_ids.entry(span).or_insert(next);
        if id == next {
            self.spans.push(span);
        }
        LineRow::source_loc(id)
    }

    /// The statements that [`Self::source_loc`] gave marks for, in the order of the marks.
    pub(super) fn take_spans(&mut self) -> Vec<MirSpan> {
        std::mem::take(&mut self.spans)
    }

    pub(super) fn finish(self) -> (Requests, Vec<(String, u64, u64)>) {
        let (wants, layouts) = self.detach();
        (wants.requests, layouts)
    }

    /// What the function asks for, with the layouts `BLAZE_LAYOUTS` collects: all that is
    /// kept of this without the database.
    pub(super) fn detach(self) -> (Wants, Vec<(String, u64, u64)>) {
        let mut layouts: Vec<(String, u64, u64)> = self.layouts.map_or_else(Vec::new, |layouts| {
            layouts
                .into_inner()
                .into_iter()
                // alloc: a debug dump, only with `BLAZE_LAYOUTS` set
                .map(|(ty, (size, align))| (format!("{ty:?}"), size, align))
                .collect() // alloc: as above
        });
        layouts.extend(self.coroutine_layouts.into_inner());
        (self.wants, layouts)
    }

    /// The function to call for `def` instantiated with `args`.
    pub(super) fn callee(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        env: ParamEnvAndCrate<'db>,
        sig: &Signature,
    ) -> Result<FuncId> {
        // Lifetimes change nothing about the code, and the ones a caller passes on are its
        // own parameters, which mean nothing in the callee.
        let interner = DbInterner::new_with(self.db, env.krate);
        let args = super::erase_regions(interner, args);
        let instance = Instance { def, args: args.store(), checked: self.checked };
        Ok(self.func(FuncTarget::Instance(instance, env.store()), sig))
    }

    pub(super) fn vtable(
        &self,
        key: &(StoredTy, Option<(TraitId, StoredGenericArgs)>),
    ) -> Option<DataId> {
        self.vtable_of(vtable_hash(&key.0, &key.1), &key.0, &key.1)
    }

    fn vtable_of(
        &self,
        hash: u64,
        ty: &StoredTy,
        trait_ref: &Option<(TraitId, StoredGenericArgs)>,
    ) -> Option<DataId> {
        let datas = &self.wants.requests.datas;
        let found = self.vtables.find(hash, |it| {
            it.hash == hash
                && matches!(
                    &datas[it.at as usize],
                    DataRequest::Vtable { ty: theirs, trait_ref: their_ref, .. }
                        if theirs == ty && their_ref == trait_ref
                )
        });
        found.map(|it| DataId::from_u32(it.at))
    }

    /// The vtable of `key`, which [`Self::vtable`] found no vtable of.
    pub(super) fn define_vtable(
        &mut self,
        key: (StoredTy, Option<(TraitId, StoredGenericArgs)>),
        blob: ConstBlob,
    ) -> DataId {
        let hash = vtable_hash(&key.0, &key.1);
        self.add_vtable(hash, key.0, key.1, blob)
    }

    fn add_vtable(
        &mut self,
        hash: u64,
        ty: StoredTy,
        trait_ref: Option<(TraitId, StoredGenericArgs)>,
        blob: ConstBlob,
    ) -> DataId {
        let at = self.wants.requests.datas.len() as u32;
        self.vtables.insert_unique(hash, Slot { hash, at }, |it| it.hash);
        self.data(DataRequest::Vtable { ty, trait_ref, blob })
    }

    pub(super) fn static_data(&mut self, id: StaticId) -> DataId {
        let datas = &mut self.wants.requests.datas;
        *self.statics.entry(id).or_insert_with(|| {
            datas.push(DataRequest::Static(id));
            DataId::from_u32(datas.len() as u32 - 1)
        })
    }

    pub(super) fn location(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> DataId {
        self.data(DataRequest::Location(at))
    }

    pub(super) fn declaration(&mut self, func: FunctionId) -> DataId {
        self.data(DataRequest::Declaration(func))
    }

    pub(super) fn file_path(&mut self, file: FileId) -> DataId {
        self.data(DataRequest::FilePath(file))
    }

    pub(super) fn text(&mut self, parts: Vec<TextPart>) -> DataId {
        self.data(DataRequest::Text(parts))
    }

    pub(super) fn target_config(&self) -> cranelift_codegen::isa::TargetFrontendConfig {
        let config = self.isa.frontend_config();
        // The ISA is only there for its calling convention: WebAssembly's pointers are 32 bits.
        #[cfg(feature = "wasm")]
        let config = cranelift_codegen::isa::TargetFrontendConfig {
            pointer_width: target_lexicon::PointerWidth::U32,
            ..config
        };
        config
    }

    /// Puts what `body` asks for among what this function asks for, and gives `body` as it
    /// is in this function's names, with the bodies it shows LLVM moved among this one's.
    fn moved(&mut self, body: &Inlinable) -> ir::Function {
        let (funcs, datas) = self.moved_requests(&body.requests);
        let rename = |func: &ir::Function| renamed(func, &funcs, &datas);
        for seen in &body.seen {
            let id = funcs[seen.id.as_u32() as usize];
            if !self.seen.iter().any(|it| it.id == id) {
                let func = rename(&seen.func);
                // clone: this function's own copy of a body it shows LLVM, once per body
                self.seen.push(Seen { id, func, pointees: seen.pointees.clone(), hint: seen.hint });
            }
        }
        rename(&body.func)
    }

    /// Keeps of what the function asks for only what `code` refers to, directly or through
    /// the data it refers to, renumbered in `code`. What the bodies it imported ask for is
    /// asked for before LLVM builds them in (see [`super::imports`]), and the most of it is
    /// of calls LLVM then takes out: each would be compiled, and whatever that calls.
    #[cfg(feature = "llvm")]
    pub(super) fn keep_referenced(&mut self, code: &mut Code) {
        let Some(kept) = self.wants.kept(code) else { return };
        let wants = &mut self.wants;
        let Requests { funcs: kept_funcs, datas: kept_datas } = &mut wants.requests;
        // In place, in their order: `retain` sees each once, first to last.
        let mut at = 0;
        kept_funcs.retain(|_| {
            at += 1;
            kept.funcs[at - 1].is_some()
        });
        let mut at = 0;
        kept_datas.retain(|_| {
            at += 1;
            kept.datas[at - 1].is_some()
        });
        for request in kept_datas.iter_mut() {
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } = request {
                kept.renumber(blob);
            }
        }
        wants.funcs.clear();
        for (at, it) in kept_funcs.iter().enumerate() {
            let hash = func_hash(it.target.as_ref(), &it.sig);
            wants.funcs.insert_unique(hash, Slot { hash, at: at as u32 }, |it| it.hash);
        }
        // A blob kept points only at what is kept, so it hashes as it did with the numbers
        // it has now.
        wants.shared.clear();
        self.vtables.clear();
        for (at, request) in kept_datas.iter().enumerate() {
            let at = at as u32;
            match request {
                DataRequest::Blob { blob, identity: DataIdentity::Shared } => {
                    let hash = BlobView { blob, datas: None, funcs: None }.hash();
                    wants.shared.insert_unique(hash, Slot { hash, at }, |it| it.hash);
                }
                DataRequest::Vtable { ty, trait_ref, .. } => {
                    let hash = vtable_hash(ty, trait_ref);
                    self.vtables.insert_unique(hash, Slot { hash, at }, |it| it.hash);
                }
                _ => {}
            }
        }
        self.statics.retain(|_, id| kept.data(*id).map(|it| *id = it).is_some());
    }

    /// Puts `requests`, another function's, among what this function asks for: what each
    /// of them is here. Each blob of `requests` points only at what came before it
    /// ([`Inlinable::of`]).
    pub(super) fn moved_requests(&mut self, requests: &Requests) -> (Vec<FuncId>, Vec<DataId>) {
        let funcs: Vec<FuncId> =
            // alloc: once per body moved in (inlined, imported or a unit's), cloned whole; exact
            requests.funcs.iter().map(|it| self.func_ref(it.target.as_ref(), &it.sig)).collect();
        let mut datas: Vec<DataId> = Vec::with_capacity(requests.datas.len());
        for request in &requests.datas {
            // `Inlinable::of` saw to it that a blob points only at what came before it.
            let id = match request {
                DataRequest::Blob { blob, identity } => {
                    let view = BlobView { blob, datas: Some(&datas[..]), funcs: Some(&funcs[..]) };
                    self.wants.define_view_as(view, *identity)
                }
                DataRequest::Vtable { ty, trait_ref, blob } => {
                    let hash = vtable_hash(ty, trait_ref);
                    match self.vtable_of(hash, ty, trait_ref) {
                        Some(id) => id,
                        None => {
                            let view =
                                BlobView { blob, datas: Some(&datas[..]), funcs: Some(&funcs[..]) };
                            let blob = view.made();
                            // clone: refcounts, once per vtable not asked for before
                            self.add_vtable(hash, ty.clone(), trait_ref.clone(), blob)
                        }
                    }
                }
                DataRequest::Static(id) => self.static_data(*id),
                DataRequest::Location(at) => self.location(*at),
                DataRequest::Declaration(func) => self.declaration(*func),
                DataRequest::FilePath(file) => self.file_path(*file),
                // clone: the request's own parts: a text is one object per body moved in
                DataRequest::Text(parts) => self.text(parts.clone()),
            };
            datas.push(id);
        }
        (funcs, datas)
    }

    /// Declares in `func` a callee whose body is put where it is called: `body`, with what
    /// it asked for asked for here.
    pub(super) fn declare_inlined(
        &mut self,
        instance: &Instance,
        body: &Inlinable,
        func: &mut ir::Function,
    ) -> ir::FuncRef {
        for it in std::iter::once(instance).chain(&body.built_in) {
            let hash = FixedState.hash_one(it);
            let built_in = &mut self.built_in;
            let entry = self.built_in_at.entry(
                hash,
                |slot| slot.hash == hash && built_in.get(slot.at as usize) == Some(it),
                |slot| slot.hash,
            );
            if let Entry::Vacant(slot) = entry {
                slot.insert(Slot { hash, at: built_in.len() as u32 });
                // clone: refcounts, once per instance built in
                built_in.push(it.clone());
            }
        }
        let hash = FixedState.hash_one(instance);
        let reused = self.reused.find(hash, |it| it.hash == hash && it.instance == *instance);
        let at = match reused {
            Some(it) => it.at,
            None => {
                let at = self.inlined.len() as u32;
                let callee = self.moved(body);
                self.inlined.push(callee);
                // clone: refcounts, once per callee built in, when its body is first moved in
                let reused = Reused { hash, instance: instance.clone(), at };
                self.reused.insert_unique(hash, reused, |it| it.hash);
                at
            }
        };
        // clone: Cranelift's `import_signature` takes it by value into the function's own table
        let signature = func.import_signature(self.inlined[at as usize].signature.clone());
        let name = func
            .declare_imported_user_function(ir::UserExternalName { namespace: INLINED, index: at });
        func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::user(name),
            signature,
            colocated: true,
            patchable: false,
        })
    }

    /// Shows LLVM `body` as what the requested function `id` does: see [`Seen`].
    pub(super) fn declare_seen(&mut self, id: FuncId, body: &Inlinable, hint: Hint) {
        if self.seen.iter().any(|it| it.id == id) {
            return;
        }
        let func = self.moved(body);
        // clone: this function's own copy of a body it shows LLVM, once per body
        self.seen.push(Seen { id, func, pointees: body.pointees.clone(), hint });
    }

    pub(super) fn declare_data_in_func(
        &self,
        id: DataId,
        func: &mut ir::Function,
    ) -> ir::GlobalValue {
        let request = &self.requests.datas[id.as_u32() as usize];
        let tls = match request {
            DataRequest::Static(id) => super::is_thread_local(self.db, *id),
            _ => false,
        };
        let name = func.declare_imported_user_function(ir::UserExternalName {
            namespace: 1,
            index: id.as_u32(),
        });
        func.create_global_value(ir::GlobalValueData::Symbol {
            name: ir::ExternalName::user(name),
            offset: ir::immediates::Imm64::new(0),
            // Only a static can be another object's.
            colocated: self.within_image() || !matches!(request, DataRequest::Static(_)),
            tls,
        })
    }
}

/// The number each request kept has among those kept ([`Wants::kept`]).
#[cfg(feature = "llvm")]
struct Kept {
    funcs: Vec<Option<u32>>,
    datas: Vec<Option<u32>>,
}

#[cfg(feature = "llvm")]
impl Kept {
    fn func(&self, id: FuncId) -> Option<FuncId> {
        self.funcs[id.as_u32() as usize].map(FuncId::from_u32)
    }

    fn data(&self, id: DataId) -> Option<DataId> {
        self.datas[id.as_u32() as usize].map(DataId::from_u32)
    }

    /// What a blob kept points to, in the numbers of what is kept.
    fn renumber(&self, blob: &mut ConstBlob) {
        blob.relocs.iter_mut().for_each(|it| it.1 = self.data(it.1).unwrap());
        blob.fn_relocs.iter_mut().for_each(|it| it.1 = self.func(it.1).unwrap());
    }
}

/// `func` in the names of the requests its own became, as [`Recorder::moved_requests`]
/// gives them.
pub(super) fn renamed(func: &ir::Function, funcs: &[FuncId], datas: &[DataId]) -> ir::Function {
    // clone: the copy is what is given, once per body moved in; the original stays its owner's
    let mut func = func.clone();
    for at in 0..func.params.user_named_funcs().len() {
        let name = ir::UserExternalNameRef::from_u32(at as u32);
        let ir::UserExternalName { namespace, index } = func.params.user_named_funcs()[name];
        let index = match namespace {
            0 => funcs[index as usize].as_u32(),
            1 => datas[index as usize].as_u32(),
            // A callee that was put in this one, which nothing calls any more.
            _ => continue,
        };
        func.params.reset_user_func_name(name, ir::UserExternalName { namespace, index });
    }
    func
}

impl From<cranelift_module::ModuleError> for CodegenError {
    fn from(e: cranelift_module::ModuleError) -> Self {
        // alloc: the error's message
        CodegenError::Backend(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use cranelift_codegen::ir::{AbiParam, types};

    use super::*;

    const KINDS: [binemit::Reloc; 6] = [
        binemit::Reloc::Abs8,
        binemit::Reloc::X86PCRel4,
        binemit::Reloc::X86CallPLTRel4,
        binemit::Reloc::Arm64Call,
        binemit::Reloc::Aarch64AdrPrelPgHi21,
        binemit::Reloc::Aarch64AddAbsLo12Nc,
    ];

    fn module_relocs(seed: u64, count: usize) -> (Vec<ModuleReloc>, Vec<(u32, u32)>) {
        let mut random = SplitMix64::new(seed);
        let mut relocs = Vec::new();
        let mut elf_types = Vec::new();
        for at in 0..count {
            let offset = at as u32 * 4;
            let name = match random.below(6) {
                0 => ModuleRelocTarget::user(0, random.below(40) as u32),
                1 => ModuleRelocTarget::user(1, random.below(40) as u32),
                5 => {
                    let namespace = [INLINED, u32::MAX - 1, 7][random.below(3) as usize];
                    ModuleRelocTarget::user(namespace, random.below(40) as u32)
                }
                2 => {
                    let all = ir::LibCall::all_libcalls();
                    ModuleRelocTarget::LibCall(all[random.below(all.len() as u64) as usize])
                }
                3 => ModuleRelocTarget::KnownSymbol(match random.below(2) {
                    0 => ir::KnownSymbol::ElfGlobalOffsetTable,
                    _ => ir::KnownSymbol::CoffTlsIndex,
                }),
                _ => ModuleRelocTarget::FunctionOffset(unstitched(), random.below(4096) as u32),
            };
            let addend = match random.below(6) {
                0 => i64::from(i32::MIN),
                1 => i64::from(i32::MAX) + 1 + random.below(1 << 20) as i64,
                2 => -(1 << 40),
                _ => random.below(64) as i64 - 32,
            };
            let kind = KINDS[random.below(KINDS.len() as u64) as usize];
            if random.below(4) == 0 {
                elf_types.push((offset, 278 + random.below(4) as u32));
            }
            relocs.push(ModuleReloc { offset, kind, name, addend });
        }
        (relocs, elf_types)
    }

    fn same_module(a: &ModuleReloc, b: &ModuleReloc) -> bool {
        let names = match (&a.name, &b.name) {
            (
                ModuleRelocTarget::User { namespace, index },
                ModuleRelocTarget::User { namespace: theirs, index: their_index },
            ) => namespace == theirs && index == their_index,
            (ModuleRelocTarget::LibCall(a), ModuleRelocTarget::LibCall(b)) => a == b,
            (ModuleRelocTarget::KnownSymbol(a), ModuleRelocTarget::KnownSymbol(b)) => a == b,
            (ModuleRelocTarget::FunctionOffset(a, x), ModuleRelocTarget::FunctionOffset(b, y)) => {
                a == b && x == y
            }
            _ => false,
        };
        names && a.offset == b.offset && a.kind == b.kind && a.addend == b.addend
    }

    /// How `Code` hashed its relocations and ELF types when it kept them as `ModuleReloc`s and
    /// a `Vec<(u32, u32)>`, which code in the store is keyed by.
    fn old_hash(relocs: &[ModuleReloc], elf_types: &Vec<(u32, u32)>) -> u64 {
        let mut state = FixedHasher::default();
        elf_types.hash(&mut state);
        relocs.len().hash(&mut state);
        for reloc in relocs {
            (reloc.offset, std::mem::discriminant(&reloc.kind), reloc.addend).hash(&mut state);
            std::mem::discriminant(&reloc.name).hash(&mut state);
            match &reloc.name {
                ModuleRelocTarget::User { namespace, index } => (namespace, index).hash(&mut state),
                ModuleRelocTarget::LibCall(call) => call.hash(&mut state),
                ModuleRelocTarget::KnownSymbol(symbol) => symbol.hash(&mut state),
                ModuleRelocTarget::FunctionOffset(func, offset) => (func, offset).hash(&mut state),
            }
        }
        state.finish()
    }

    #[test]
    fn a_relocation_is_sixteen_bytes() {
        assert_eq!(size_of::<Row>(), 16);
        assert_eq!(size_of::<binemit::Reloc>(), 1);
    }

    #[test]
    fn relocations_give_back_what_they_were_given_and_hash_as_before() {
        for seed in 0..64 {
            let (relocs, elf_types) = module_relocs(seed, seed as usize * 3);
            let rows = Relocs::of_module(&relocs, &elf_types).unwrap();
            assert_eq!(rows.len(), relocs.len());
            for (at, (row, was)) in iter::zip(rows.iter(), &relocs).enumerate() {
                assert!(same_module(&row.module(unstitched()), was), "seed {seed}, at {at}");
                assert_eq!(rows.get(at), row);
                let typed = elf_types.iter().find(|it| it.0 == was.offset).map(|it| it.1);
                assert_eq!(row.elf_type, typed);
            }
            let mut state = FixedHasher::default();
            rows.hash_elf_types(&mut state);
            rows.hash_as_module(&mut state);
            assert_eq!(state.finish(), old_hash(&relocs, &elf_types), "seed {seed}");
        }
    }

    #[test]
    fn relocations_are_renumbered_and_shifted_in_place() {
        let (relocs, elf_types) = module_relocs(7, 50);
        let mut rows = Relocs::of_module(&relocs, &elf_types).unwrap();
        rows.renumber(|it| it + 1000, |it| it * 2);
        rows.shift(16);
        for (row, was) in iter::zip(rows.iter(), &relocs) {
            let name = match was.name {
                ModuleRelocTarget::User { namespace: 0, index } => RelocName::Func(index + 1000),
                ModuleRelocTarget::User { namespace: 1, index } => RelocName::Data(index * 2),
                ModuleRelocTarget::User { namespace, index } => RelocName::User { namespace, index },
                ref other => RelocName::of_module(other).unwrap(),
            };
            assert_eq!((row.offset, row.name, row.addend), (was.offset + 16, name, was.addend));
        }
    }

    #[test]
    fn what_a_row_cannot_hold_is_an_error() {
        let reloc = |name| ModuleReloc { offset: 0, kind: KINDS[0], name, addend: 0 };
        let plain = reloc(ModuleRelocTarget::user(0, 0));
        let wide = [(0, 1 << 16)];
        assert_eq!(Relocs::of_module(&[plain.clone()], &wide), Err(RelocError::ElfType(1 << 16)));
        let elsewhere = [(4, 278)];
        assert_eq!(Relocs::of_module(&[plain], &elsewhere), Err(RelocError::Unmatched(4)));
    }

    fn signature(params: usize) -> Signature {
        let mut sig = Signature::new(cranelift_codegen::isa::CallConv::SystemV);
        sig.params.extend((0..params).map(|_| AbiParam::new(types::I64)));
        sig
    }

    fn target(which: u64) -> FuncTarget {
        match which % 3 {
            0 => FuncTarget::Symbol(format!("s{}", which / 3)),
            1 => FuncTarget::Path(format!("p{}", which / 3)),
            _ => FuncTarget::Asm(format!("a{}", which / 3)),
        }
    }

    /// The tables of [`Wants`] give every request the number a search of the requests made
    /// before gives it, as the map of whole requests to numbers did.
    #[test]
    fn requests_are_numbered_as_a_search_of_those_before_numbers_them() {
        let isa = &**crate::target().unwrap();
        let mut random = SplitMix64::new(11);
        let mut wants = Wants::new(isa);
        let mut model: Vec<(FuncTarget, Signature)> = Vec::new();
        for _ in 0..2000 {
            let (target, sig) = (target(random.below(60)), signature(random.below(3) as usize));
            let expected = match model.iter().position(|it| it.0 == target && it.1 == sig) {
                Some(at) => at,
                None => {
                    model.push((target.clone(), sig.clone()));
                    model.len() - 1
                }
            };
            let by_name = random.below(2) == 0;
            let id = match target {
                FuncTarget::Symbol(name) if by_name => wants.import(&name, &sig).unwrap(),
                FuncTarget::Path(path) if by_name => wants.import_path(&path, &sig),
                target => wants.func(target, &sig),
            };
            assert_eq!(id.as_u32() as usize, expected);
        }
        assert_eq!(wants.requests.funcs.len(), model.len());

        let blob = |random: &mut SplitMix64| ConstBlob {
            bytes: vec![random.below(4) as u8; 1 + random.below(3) as usize],
            relocs: (0..random.below(2)).map(|_| (0, DataId::from_u32(0), 8)).collect(),
            align: 1 << random.below(2),
            ..ConstBlob::default()
        };
        let mut shared: Vec<(ConstBlob, usize)> = Vec::new();
        wants.define_data_as(ConstBlob::default(), DataIdentity::Own);
        for _ in 0..500 {
            let (blob, own) = (blob(&mut random), random.below(5) == 0);
            let before = wants.requests.datas.len();
            if own {
                let id = wants.define_data_as(blob, DataIdentity::Own);
                assert_eq!(id.as_u32() as usize, before);
                continue;
            }
            let expected = match shared.iter().find(|it| it.0 == blob) {
                Some(it) => it.1,
                None => {
                    shared.push((blob.clone(), before));
                    before
                }
            };
            assert_eq!(wants.define_data_as(blob, DataIdentity::Shared).as_u32() as usize, expected);
        }
    }

    /// A blob renamed in place hashes and compares as the blob it is made into.
    #[test]
    fn a_renamed_blob_is_the_blob_it_is_made_into() {
        let mut random = SplitMix64::new(3);
        for _ in 0..200 {
            let blob = ConstBlob {
                bytes: (0..random.below(16)).map(|_| random.below(256) as u8).collect(),
                relocs: (0..random.below(4))
                    .map(|_| (random.below(8) as u32, DataId::from_u32(random.below(4) as u32), 0))
                    .collect(),
                fn_relocs: (0..random.below(3))
                    .map(|_| (random.below(8) as u32, FuncId::from_u32(random.below(4) as u32)))
                    .collect(),
                align: 8,
                type_ids: Vec::new(),
            };
            let datas: Vec<DataId> = (0..4).map(|_| DataId::from_u32(random.below(9) as u32)).collect();
            let funcs: Vec<FuncId> = (0..4).map(|_| FuncId::from_u32(random.below(9) as u32)).collect();
            let view = BlobView { blob: &blob, datas: Some(&datas[..]), funcs: Some(&funcs[..]) };
            let made = view.made();
            let plain = BlobView { blob: &made, datas: None, funcs: None };
            assert_eq!(view.hash(), plain.hash());
            assert!(view.is(&made) && plain.is(&made));
            let unrenamed = BlobView { blob: &blob, datas: None, funcs: None };
            assert_eq!(unrenamed.is(&made), blob == made);
        }
    }
}
