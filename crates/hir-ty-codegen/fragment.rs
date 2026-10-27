//! What compiling one function gives: machine code that names the functions and data it uses
//! by what they are, not by where an object file put them. That makes the result a function of
//! the database alone, so it is a query, and a build stitches the results into an object.

use std::{
    cell::RefCell,
    hash::{Hash, Hasher},
    iter,
};

use base_db::FileId;
use cranelift_codegen::{
    ir::{self, Signature},
    isa::TargetIsa,
};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_module::{DataId, FuncId, ModuleReloc, ModuleRelocTarget};
use hir_def::{
    DefWithBodyId, FunctionId, StaticId, TraitId,
    hir::{BindingId, ExprId},
};
use rustc_hash::FxHashMap;

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
#[derive(Clone, PartialEq, Eq, Hash)]
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

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct FuncRequest {
    pub(super) target: FuncTarget,
    pub(super) sig: Signature,
}

/// A data object that compiled code or other data points to. The ids inside a blob index the
/// requests of the same [`Requests`].
#[derive(Clone, PartialEq, Eq, Hash)]
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
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum TextPart {
    Text(String),
    /// The path of a file as rustc reports it.
    FilePath(FileId),
}

/// Everything a compiled function or static refers to. `FuncId` and `DataId` index these
/// until the stitcher replaces them with the ids of its module.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub(super) struct Requests {
    pub(super) funcs: Vec<FuncRequest>,
    pub(super) datas: Vec<DataRequest>,
    /// Whether the code has where something is written in it, which moving that changes and
    /// no request stands for: the value of a `line!()`, `column!()` or `file!()`, or the path
    /// of a type `type_name` is of.
    pub(super) placed: bool,
}

#[derive(Clone)]
pub(super) struct Code {
    pub(super) signature: Signature,
    pub(super) bytes: Vec<u8>,
    pub(super) align: u64,
    pub(super) relocs: Vec<ModuleReloc>,
    /// The relocations of `relocs`, by where they are, that Cranelift has no kind for, with
    /// the type ELF has for each: the low bits of an address in an AArch64 load or store,
    /// which go in in units of what it moves, and whatever the assembly of a function
    /// names a symbol with. The kind in `relocs` stands in for it, and the object is given
    /// the relocation when it is finished.
    pub(super) elf_types: Vec<(u32, u32)>,
    pub(super) unwind: Option<FunctionUnwind>,
    /// Which statement the code is of, from each offset where that changes.
    pub(super) lines: Vec<LineRow>,
    /// Where the slots of named locals are, for a debugger.
    pub(super) slots: SlotPlaces,
}

impl PartialEq for Code {
    fn eq(&self, other: &Code) -> bool {
        let same_target = |a: &ModuleRelocTarget, b: &ModuleRelocTarget| match (a, b) {
            (
                ModuleRelocTarget::User { namespace, index },
                ModuleRelocTarget::User { namespace: other_namespace, index: other_index },
            ) => namespace == other_namespace && index == other_index,
            (ModuleRelocTarget::LibCall(a), ModuleRelocTarget::LibCall(b)) => a == b,
            (ModuleRelocTarget::KnownSymbol(a), ModuleRelocTarget::KnownSymbol(b)) => a == b,
            (
                ModuleRelocTarget::FunctionOffset(func, offset),
                ModuleRelocTarget::FunctionOffset(other_func, other_offset),
            ) => func == other_func && offset == other_offset,
            _ => false,
        };
        self.bytes == other.bytes
            && self.align == other.align
            && self.signature == other.signature
            && self.unwind == other.unwind
            && self.lines == other.lines
            && self.slots == other.slots
            && self.elf_types == other.elf_types
            && self.relocs.len() == other.relocs.len()
            && iter::zip(&self.relocs, &other.relocs).all(|(a, b)| {
                a.offset == b.offset
                    && a.kind == b.kind
                    && a.addend == b.addend
                    && same_target(&a.name, &b.name)
            })
    }
}

/// Compared by the database when a function is compiled again: one that came out the same
/// leaves what was built from it alone.
#[derive(Clone, PartialEq, Hash)]
pub struct CompiledFunction {
    pub(super) requests: Requests,
    pub(super) code: Code,
    /// The body the function was compiled from and the statements of it that
    /// [`Code::lines`] index. Looked up when stitching, like a [`DataRequest::Location`].
    pub(super) source: Option<(DefWithBodyId, Vec<MirSpan>)>,
    /// The closure or `async` block of that body the code is of, when it is of one.
    pub(super) closure: Option<ExprId>,
    /// The named locals of the body a debugger can be told of, whose types are of `.0`.
    pub(super) variables: Option<(StoredParamEnvAndCrate, Vec<Variable>)>,
    /// Size and alignment of every type laid out, collected when `RA_JIT_LAYOUTS` is set.
    pub(super) layouts: Vec<(String, u64, u64)>,
}

/// A local written with a name that is kept in one slot of the frame for as long as the
/// function runs.
#[derive(Clone, PartialEq, Hash)]
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
        let Code { signature, bytes, align, relocs, elf_types, unwind, lines, slots: places } =
            self;
        (signature, bytes, align, elf_types, lines).hash(state);
        if slots {
            places.hash(state);
        }
        // Cranelift's unwind rules have no `Hash`: what they print is all of them.
        format!("{unwind:?}").hash(state);
        relocs.len().hash(state);
        for reloc in relocs {
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

/// How many requests a [`Recorder`] had at some point, to tell the ones made since.
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "llvm"), expect(dead_code))]
pub(super) struct Mark {
    pub(super) funcs: usize,
    pub(super) datas: usize,
}

/// A function built to be put where it is called, and what it refers to.
#[derive(PartialEq)]
pub(super) struct Inlinable {
    pub(super) func: ir::Function,
    pub(super) requests: Requests,
    /// What its parameters point to, as [`Recorder::pointees`].
    pub(super) pointees: Vec<(u32, Pointee)>,
    /// The bodies of what it calls that LLVM is shown, as [`Recorder::seen`].
    pub(super) seen: Vec<Seen>,
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
#[derive(Clone, PartialEq)]
pub(super) struct Seen {
    /// The function the calls are of, among the requests.
    pub(super) id: FuncId,
    pub(super) func: ir::Function,
    pub(super) pointees: Vec<(u32, Pointee)>,
    pub(super) hint: Hint,
}

/// What a function's `#[inline]` asks of LLVM where its body is shown, as rustc asks it:
/// `inlinehint`, `alwaysinline` or `noinline`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hint {
    None,
    Inline,
    Always,
    Never,
}

/// The namespace of the names of callees that are built into their callers. No relocation
/// has it, as no call of one is left when the caller is compiled.
pub(super) const INLINED: u32 = u32::MAX;

/// Collects the requests of one function while its IR is built.
pub(super) struct Recorder<'db> {
    pub(super) db: &'db dyn HirDatabase,
    pub(super) isa: &'static dyn TargetIsa,
    /// What `isa` generates code for.
    pub(super) target: &'static Target,
    /// Whether the crate compiling the function has `-C overflow-checks`.
    pub(super) checked: bool,
    /// What it asks for, which is all a producer of its code needs of this.
    pub(super) wants: Wants,
    vtables: FxHashMap<(StoredTy, Option<(TraitId, StoredGenericArgs)>), DataId>,
    statics: FxHashMap<StaticId, DataId>,
    pub(super) layouts: Option<RefCell<FxHashMap<Ty<'db>, (u64, u64)>>>,
    /// With `layouts`: each `async fn`'s future as rustc names it, with the offsets of what
    /// each of its variants holds, for `harness/layoutdiff.py` to set against rustc's.
    pub(super) coroutine_layouts: RefCell<Vec<(String, u64, u64)>>,
    pub(super) builder_ctx: FunctionBuilderContext,
    /// The assembly of the function, when it is a `#[naked]` one, and what each operand of
    /// it that is a `sym` names.
    pub(super) naked: Option<(String, Vec<Option<ModuleRelocTarget>>)>,
    /// The statements instructions were built for, which their source locations index.
    spans: Vec<MirSpan>,
    span_ids: FxHashMap<MirSpan, u32>,
    /// The locals written with a name that a stack slot of their own tells where they are.
    pub(super) named: Vec<Named>,
    /// The bodies of the callees that are built into their callers, which call them by their
    /// place here in [`INLINED`] until [`inline`](super::inline) puts the bodies there.
    pub(super) inlined: Vec<ir::Function>,
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
    pub(super) isa: &'static dyn TargetIsa,
    pub(super) requests: Requests,
    funcs: FxHashMap<FuncRequest, FuncId>,
    shared: FxHashMap<ConstBlob, DataId>,
    /// The parameters of the function that are addresses of memory its signature says
    /// something of, by their place among the parameters of its IR.
    pub(super) pointees: Vec<(u32, Pointee)>,
    /// The bodies of functions it calls that LLVM is shown, in its names.
    pub(super) seen: Vec<Seen>,
}

impl Wants {
    pub(super) fn new(isa: &'static dyn TargetIsa) -> Wants {
        Wants {
            isa,
            requests: Requests::default(),
            funcs: FxHashMap::default(),
            shared: FxHashMap::default(),
            pointees: Vec::new(),
            seen: Vec::new(),
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
    pub(super) fn asm_blocks(&self) -> FxHashMap<u32, std::sync::Arc<super::asm::Inline>> {
        let funcs = self.requests.funcs.iter().enumerate();
        funcs
            .filter_map(|(at, it)| match &it.target {
                FuncTarget::Asm(text) => Some((at as u32, super::asm::inline_of(text)?)),
                _ => None,
            })
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
        let mut funcs = vec![false; requests.funcs.len()];
        let mut datas = vec![false; requests.datas.len()];
        let mut reached = Vec::new();
        for reloc in &code.relocs {
            match reloc.name {
                ModuleRelocTarget::User { namespace: 0, index } => funcs[index as usize] = true,
                ModuleRelocTarget::User { namespace: 1, index } => reached.push(index as usize),
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
            numbers.collect::<Vec<Option<u32>>>()
        };
        let kept = Kept { funcs: numbered(&funcs), datas: numbered(&datas) };
        for reloc in &mut code.relocs {
            if let ModuleRelocTarget::User { namespace, index } = &mut reloc.name {
                *index = match *namespace {
                    0 => kept.funcs[*index as usize].unwrap(),
                    1 => kept.datas[*index as usize].unwrap(),
                    _ => *index,
                };
            }
        }
        Some(kept)
    }

    /// Of what the functions of a unit ask for, what `code`, one function's, refers to, in
    /// its own numbers, which `code` is renumbered to.
    #[cfg(feature = "llvm")]
    pub(super) fn referenced(&self, code: &mut Code) -> Requests {
        let Some(kept) = self.kept(code) else {
            return self.requests.clone();
        };
        let funcs = iter::zip(&self.requests.funcs, &kept.funcs);
        let funcs = funcs.filter(|it| it.1.is_some()).map(|it| it.0.clone()).collect();
        let datas = iter::zip(&self.requests.datas, &kept.datas).filter(|it| it.1.is_some());
        let datas = datas.map(|(request, _)| {
            let mut request = request.clone();
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } = &mut request
            {
                kept.renumber(blob);
            }
            request
        });
        Requests { funcs, datas: datas.collect(), placed: self.requests.placed }
    }

    pub(super) fn func(&mut self, target: FuncTarget, sig: &Signature) -> FuncId {
        let request = FuncRequest { target, sig: sig.clone() };
        if let Some(&id) = self.funcs.get(&request) {
            return id;
        }
        let id = FuncId::from_u32(self.requests.funcs.len() as u32);
        self.requests.funcs.push(request.clone());
        self.funcs.insert(request, id);
        id
    }

    fn data(&mut self, request: DataRequest) -> DataId {
        let id = DataId::from_u32(self.requests.datas.len() as u32);
        self.requests.datas.push(request);
        id
    }

    pub(super) fn import(&mut self, symbol: &str, sig: &Signature) -> Result<FuncId> {
        Ok(self.func(FuncTarget::Symbol(symbol.to_owned()), sig))
    }

    pub(super) fn import_path(&mut self, path: &str, sig: &Signature) -> FuncId {
        self.func(FuncTarget::Path(path.to_owned()), sig)
    }

    pub(super) fn assembly(&mut self, text: String, sig: &Signature) -> FuncId {
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
        if let Some(&id) = self.shared.get(&blob) {
            return id;
        }
        let id = self.data(DataRequest::Blob { blob: blob.clone(), identity });
        self.shared.insert(blob, id);
        id
    }

    pub(super) fn make_signature(&self) -> Signature {
        Signature::new(self.isa.default_call_conv())
    }

    /// Whether what another object defines is reached as what this one does: the linker of
    /// a PE image puts a stub where an import is called, and there is no other way.
    fn within_image(&self) -> bool {
        self.isa.triple().binary_format == target_lexicon::BinaryFormat::Coff
    }

    /// Whether a call names its callee in place, for the linker to send through a stub of
    /// its own when the callee turns out to be another library's, rather than load the
    /// callee's address from a slot first. An x86-64 ELF linker does that for a `call`.
    fn calls_in_place(&self) -> bool {
        let triple = self.isa.triple();
        self.within_image()
            || (triple.binary_format == target_lexicon::BinaryFormat::Elf
                && triple.architecture == target_lexicon::Architecture::X86_64)
    }

    /// Names a requested function in `func`, to call it. Whether the function ends up in
    /// this object is not known here.
    pub(super) fn declare_func_in_func(&self, id: FuncId, func: &mut ir::Function) -> ir::FuncRef {
        self.declare_func(id, func, self.calls_in_place())
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

impl<'db> Recorder<'db> {
    /// The stack slots that hold a local written with a name.
    pub(super) fn named_slots(&self) -> impl Iterator<Item = ir::StackSlot> + '_ {
        self.named.iter().map(|it| it.slot)
    }

    pub(super) fn new(
        db: &'db dyn HirDatabase,
        isa: &'static dyn TargetIsa,
        target: &'static Target,
        checked: bool,
    ) -> Self {
        Recorder {
            db,
            isa,
            target,
            checked,
            wants: Wants::new(isa),
            vtables: FxHashMap::default(),
            statics: FxHashMap::default(),
            layouts: std::env::var_os("RA_JIT_LAYOUTS").map(|_| Default::default()),
            coroutine_layouts: RefCell::default(),
            builder_ctx: FunctionBuilderContext::new(),
            naked: None,
            spans: Vec::new(),
            span_ids: FxHashMap::default(),
            named: Vec::new(),
            inlined: Vec::new(),
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

    /// What the function asks for, with the layouts `RA_JIT_LAYOUTS` collects: all that is
    /// kept of this without the database.
    pub(super) fn detach(self) -> (Wants, Vec<(String, u64, u64)>) {
        let mut layouts: Vec<(String, u64, u64)> = self.layouts.map_or_else(Vec::new, |layouts| {
            layouts
                .into_inner()
                .into_iter()
                .map(|(ty, (size, align))| (format!("{ty:?}"), size, align))
                .collect()
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
        self.vtables.get(key).copied()
    }

    pub(super) fn define_vtable(
        &mut self,
        key: (StoredTy, Option<(TraitId, StoredGenericArgs)>),
        blob: ConstBlob,
    ) -> DataId {
        let (ty, trait_ref) = key.clone();
        let id = self.data(DataRequest::Vtable { ty, trait_ref, blob });
        self.vtables.insert(key, id);
        id
    }

    pub(super) fn static_data(&mut self, id: StaticId) -> DataId {
        if let Some(&data) = self.statics.get(&id) {
            return data;
        }
        let data = self.data(DataRequest::Static(id));
        self.statics.insert(id, data);
        data
    }

    pub(super) fn location(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> DataId {
        self.data(DataRequest::Location(at))
    }

    pub(super) fn declaration(&mut self, func: FunctionId) -> DataId {
        self.data(DataRequest::Declaration(func))
    }

    pub(super) fn file_path(&mut self, file: FileId) -> DataId {
        self.requests.placed = true;
        self.data(DataRequest::FilePath(file))
    }

    pub(super) fn text(&mut self, parts: Vec<TextPart>) -> DataId {
        self.requests.placed = true;
        self.data(DataRequest::Text(parts))
    }

    /// Notes that the code has where something is written in it: see [`Requests::placed`].
    pub(super) fn placed(&mut self) {
        self.requests.placed = true;
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
        let Requests { funcs: all_funcs, datas: all_datas, placed } =
            std::mem::take(&mut self.wants.requests);
        let kept_funcs: Vec<FuncRequest> =
            iter::zip(all_funcs, &kept.funcs).filter(|it| it.1.is_some()).map(|it| it.0).collect();
        let mut kept_datas: Vec<DataRequest> =
            iter::zip(all_datas, &kept.datas).filter(|it| it.1.is_some()).map(|it| it.0).collect();
        for request in &mut kept_datas {
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } = request {
                kept.renumber(blob);
            }
        }
        let wants = &mut self.wants;
        wants.funcs = kept_funcs
            .iter()
            .enumerate()
            .map(|(at, it)| (it.clone(), FuncId::from_u32(at as u32)))
            .collect();
        wants.shared = std::mem::take(&mut wants.shared)
            .into_iter()
            .filter_map(|(mut blob, id)| {
                kept.data(id)?;
                kept.renumber(&mut blob);
                Some((blob, kept.data(id)?))
            })
            .collect();
        let still = |id: &mut DataId| kept.data(*id).map(|it| *id = it).is_some();
        self.statics.retain(|_, id| still(id));
        self.vtables.retain(|_, id| still(id));
        wants.requests = Requests { funcs: kept_funcs, datas: kept_datas, placed };
    }

    /// Puts `requests`, another function's, among what this function asks for: what each
    /// of them is here. Each blob of `requests` points only at what came before it
    /// ([`Inlinable::of`]).
    pub(super) fn moved_requests(&mut self, requests: &Requests) -> (Vec<FuncId>, Vec<DataId>) {
        self.requests.placed |= requests.placed;
        let funcs: Vec<FuncId> =
            requests.funcs.iter().map(|it| self.func(it.target.clone(), &it.sig)).collect();
        let mut datas: Vec<DataId> = Vec::with_capacity(requests.datas.len());
        for request in &requests.datas {
            // `Inlinable::of` saw to it that a blob points only at what came before it.
            let moved = |blob: &ConstBlob| ConstBlob {
                bytes: blob.bytes.clone(),
                relocs: blob
                    .relocs
                    .iter()
                    .map(|&(offset, target, addend)| {
                        (offset, datas[target.as_u32() as usize], addend)
                    })
                    .collect(),
                fn_relocs: blob
                    .fn_relocs
                    .iter()
                    .map(|&(offset, target)| (offset, funcs[target.as_u32() as usize]))
                    .collect(),
                align: blob.align,
                type_ids: blob.type_ids.clone(),
            };
            let id = match request {
                DataRequest::Blob { blob, identity } => self.define_data_as(moved(blob), *identity),
                DataRequest::Vtable { ty, trait_ref, blob } => {
                    let key = (ty.clone(), trait_ref.clone());
                    match self.vtable(&key) {
                        Some(id) => id,
                        None => self.define_vtable(key, moved(blob)),
                    }
                }
                DataRequest::Static(id) => self.static_data(*id),
                DataRequest::Location(at) => self.location(*at),
                DataRequest::Declaration(func) => self.declaration(*func),
                DataRequest::FilePath(file) => self.file_path(*file),
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
        body: &Inlinable,
        func: &mut ir::Function,
    ) -> ir::FuncRef {
        let callee = self.moved(body);
        let signature = func.import_signature(callee.signature.clone());
        let name = func.declare_imported_user_function(ir::UserExternalName {
            namespace: INLINED,
            index: self.inlined.len() as u32,
        });
        self.inlined.push(callee);
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
    let mut func = func.clone();
    let names: Vec<_> =
        func.params.user_named_funcs().iter().map(|(name, it)| (name, it.clone())).collect();
    for (name, was) in names {
        let index = match was.namespace {
            0 => funcs[was.index as usize].as_u32(),
            1 => datas[was.index as usize].as_u32(),
            // A callee that was put in this one, which nothing calls any more.
            _ => continue,
        };
        func.params
            .reset_user_func_name(name, ir::UserExternalName { namespace: was.namespace, index });
    }
    func
}

impl From<cranelift_module::ModuleError> for CodegenError {
    fn from(e: cranelift_module::ModuleError) -> Self {
        CodegenError::Backend(e.to_string())
    }
}
