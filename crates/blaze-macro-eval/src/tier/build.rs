//! A macro crate's compiled image (macros-tier.md, "Building"): every function the crate's
//! clients reach, lowered to the IR copy-and-patch compiles (`hir_ty_codegen::v2::lowered`),
//! confined ([`super::confine`]), compiled by `blaze_stencil` and loaded ([`super::load`]),
//! with a stub for each name a shim answers ([`super::bind`], [`super::glue::stub`]), an entry
//! thunk for each signature the run enters ([`super::glue::thunk`]), and the region's image
//! made of the statics' values, the functions' constants and the `Location`s they name.
//!
//! The roots are each macro's client function, `Client::expand1::<F>` or `expand2::<F>` of
//! its `#[proc_macro*]` function `F`, and the bridge's `Buffer::new`, which the run calls to
//! make the client's input buffer: what the evaluator's run calls. Everything is resolved
//! here; the loader only places and patches.

use std::sync::{Arc, LazyLock, OnceLock};

use base_db::{Crate, Db, FileId};
use blaze_codegen::{
    Backend, Data, Init as StaticInit, InstanceKey, RelocKind, Relocs, Target, Unit,
    backend::{Frontend as _, Prepare as _},
    driver::Build,
    memo::Memo,
    plan::{Mode, OptLevel, Origin, Override, Package, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Relocation, Sanitize, Spec},
};
use blaze_format::SpanRef;
use blaze_hash::{HashMap, SharedByHash};
use blaze_ids::ContentId;
use cranelift_codegen::{binemit::Reloc as Kind, ir, isa::CallConv};
use hir_def::{FunctionId, StaticId};
use hir_ty_codegen::v2::{
    Backends, BuildTime, Databases, HashSource, HirFrontend, HirInstance, HirStatic, ItemHashes,
    Lowering, Runs, lowered, span_tables_of,
};

use super::{
    bind::{self, Binding, Refusal, Use, Why},
    confine::{self, Name, Ref},
    glue,
    load::{Blob, Call, CodeReloc, Function, Image, Init, Input, To},
    machine::{self, Thunk},
    region::{Bounds, Layout, cell},
    shims::{Bound, Stub},
    table::{Sig, Signatures},
};
use crate::{guest::Stop, os::Os, run::Bridge};

/// A macro crate's image and what the run needs to enter it.
pub(crate) struct Compiled {
    pub(crate) image: Image,
    pub(crate) stubs: Vec<Stub>,
    /// Each `#[proc_macro*]` function's client function, by its function index.
    pub(crate) clients: Vec<(FunctionId, u32)>,
    /// `Buffer::new`'s function index.
    pub(crate) buffer_new: u32,
    /// The entry thunk of each signature the run enters, with the confined signature.
    thunks: Vec<(Sig, Thunk)>,
    /// Each function's table signature, by function index; `Sig::NONE` for one with none.
    sigs: Vec<Sig>,
    /// The confined signature of each [`Sig`] some entry or root has, by its id.
    signatures: HashMap<Sig, ir::Signature>,
    /// Every static, with its blob and its size.
    pub(crate) statics: Vec<(StaticId, u32, u32)>,
    /// How the bridge's `Closure::call` is passed its values, when the crate calls it.
    pub(crate) bridge: Option<BridgeAbi>,
    /// The address of the image's `resume` routine.
    pub(crate) resume: u64,
}

/// Where the bridge's `Closure::call(&mut self, buffer: Buffer) -> Buffer` has each value,
/// by its stub's slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BridgeAbi {
    /// The slot of the result's address (`sret`).
    pub(crate) ret: usize,
    /// The slot of the address of the buffer's copy.
    pub(crate) buffer: usize,
}

impl Compiled {
    /// The entry thunk of `sig`.
    pub(crate) fn thunk(&self, sig: Sig) -> Option<Thunk> {
        self.thunks.iter().find(|it| it.0 == sig).map(|it| it.1)
    }

    /// Function `n`'s table signature and its confined signature.
    pub(crate) fn signature_of(&self, n: u32) -> Option<(Sig, &ir::Signature)> {
        let sig = *self.sigs.get(n as usize)?;
        Some((sig, self.signatures.get(&sig)?))
    }

    /// The confined signature of the table signature `sig`.
    pub(crate) fn signature(&self, sig: Sig) -> Option<&ir::Signature> {
        self.signatures.get(&sig)
    }
}

/// The compiled image of the macro crate whose code hash is `code`, made once a process: one
/// compile serves every expansion of the crate's version, and a crate that can't be compiled
/// stays refused for that version, as it would be again.
pub(crate) fn compiled(
    db: &Db,
    code: ContentId,
    krate: Crate,
    bridge: &Bridge,
    macros: &[(FunctionId, bool)],
) -> Result<Arc<Compiled>, Stop> {
    type Cell = Arc<OnceLock<Result<Arc<Compiled>, Stop>>>;
    static IMAGES: LazyLock<SharedByHash<ContentId, Cell>> = LazyLock::new(Default::default);
    // clone: one reference count per expansion, so the table's guard is let go before the compile
    let cell = Arc::clone(IMAGES.pin().get_or_insert_with(code, Cell::default));
    cell.get_or_init(|| compile(db, krate, bridge, macros).map(Arc::new)).clone() // clone: an `Arc` or the crate's refusal
}

/// The C convention the host enters and is entered by.
fn host_conv() -> CallConv {
    if cfg!(all(target_arch = "aarch64", target_vendor = "apple")) {
        CallConv::AppleAarch64
    } else if cfg!(windows) {
        CallConv::WindowsFastcall
    } else {
        CallConv::SystemV
    }
}

fn codegen_stop(error: &blaze_codegen::CodegenError) -> Stop {
    // alloc: error path
    Stop(format!("can't be compiled: {}", error.what))
}

/// The run's shims, as the build binds names to them: each id an index of `bound`.
struct Claims<'a> {
    db: &'a Db,
    krate: Crate,
    closure_call: FunctionId,
    bound: std::cell::RefCell<Vec<Bound>>,
}

impl Claims<'_> {
    fn push(&self, bound: Bound) -> Option<u16> {
        let mut all = self.bound.borrow_mut();
        let id = u16::try_from(all.len()).ok()?;
        all.push(bound);
        Some(id)
    }
}

impl bind::Shims for Claims<'_> {
    fn external(&self, symbol: &str, used: Use) -> Option<u16> {
        match used {
            Use::Call => self.push(Bound::Os(Os::claims_extern(self.db, self.krate, symbol)?)),
            Use::Address => None,
        }
    }

    fn claimed(&self, func: FunctionId) -> Option<u16> {
        if func == self.closure_call {
            return self.push(Bound::Bridge);
        }
        self.push(Bound::Os(Os::claims(self.db, func)?))
    }
}

/// A code target before the stubs are numbered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Code {
    /// Reached function `n`, compiled.
    Compiled(u32),
    /// Stub `n`.
    Stub(u32),
}

/// A relocation's target before the blobs and stubs are numbered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pend {
    Code(Code),
    Pointer(Code),
    Static(u32),
    /// A blob made here: a `Location`'s file name.
    Blob(u32),
    Data(ContentId),
    Position(SpanRef),
    Guest(u32),
    Host(u64),
    Table,
    TableSize,
}

/// What a stub is made of.
struct StubPlan {
    bound: Bound,
    /// The confined signature its callers call it with; `None` while only its address is
    /// taken.
    sig: Option<ir::Signature>,
    /// Its name, for errors.
    name: String,
}

/// A compiled function before it is placed.
struct Compiledfn {
    name: String,
    code: blaze_stencil::Code,
    relocs: Vec<(u32, Kind, Pend, i64)>,
}

/// A blob before its targets are numbered.
struct PendBlob {
    align: u32,
    init: Init,
    writable: bool,
    relocs: Vec<(u32, Pend, i64)>,
}

/// The state of one build.
struct Builder<'a> {
    claims: &'a Claims<'a>,
    /// Each reached function's code, by its key in the build.
    slots: HashMap<InstanceKey, Code>,
    stubs: Vec<StubPlan>,
    /// Stubs by the symbol or instance they stand for.
    stub_of_symbol: HashMap<String, u32>,
    blobs: Vec<PendBlob>,
    data: HashMap<ContentId, u32>,
    statics: HashMap<(blaze_items::ItemKey, ContentId), u32>,
    positions: Vec<SpanRef>,
    by_sig: HashMap<Sig, ir::Signature>,
    /// Functions and stubs whose address is taken: they get table entries.
    taken: Vec<Code>,
}

impl Builder<'_> {
    fn stub(&mut self, symbol: &str, bound: Bound) -> Code {
        if let Some(&n) = self.stub_of_symbol.get(symbol) {
            return Code::Stub(n);
        }
        let n = self.stubs.len() as u32;
        // alloc: once per bound name, per crate compiled
        self.stubs.push(StubPlan { bound, sig: None, name: symbol.to_owned() });
        self.stub_of_symbol.insert(symbol.to_owned(), n); // alloc: as above
        Code::Stub(n)
    }

    /// The binding of the external `symbol`, used as `used` by a function that also calls it
    /// (`called`), as code or data.
    fn external(&mut self, symbol: &str, weak: bool, used: Use, called: bool) -> Result<Pend, Stop> {
        let binding = bind::external(symbol, used, self.claims).map_err(|it| it.stop())?;
        Ok(match binding {
            Binding::Own(own) => Pend::Code(self.stub(symbol, Bound::Own(own))),
            Binding::Shim(id) => {
                let bound = self.claims.bound.borrow_mut().get_mut(usize::from(id)).map(|it| {
                    std::mem::replace(it, Bound::Unshimmed(String::new())) // alloc: none, an empty name
                });
                let Some(bound) = bound else {
                    return Err(Stop(format!("has `{symbol}` bound to shim {id}, which the run has none of"))); // alloc: error path
                };
                Pend::Code(self.stub(symbol, bound))
            }
            Binding::Pure => match machine::host_symbol(symbol) {
                Some(address) => Pend::Host(address),
                // alloc: error path
                None => return Err(Stop(format!("calls `{symbol}`, which blaze's own process does not export"))),
            },
            // An absent weak symbol's address is null, as a linker that finds no definition
            // writes it.
            Binding::Unshimmed if weak && used == Use::Address && !called => Pend::Guest(0),
            Binding::Unshimmed => Pend::Code(self.stub(symbol, Bound::Unshimmed(symbol.to_owned()))), // alloc: once per unshimmed name
            Binding::Zero => Pend::Guest(Layout::HEADER + cell::ZERO),
        })
    }

    /// A target of the build, as code or data. `in_data`: it is the address of a function in
    /// data, its table pointer.
    fn target(&mut self, target: &Target, used: Use, called: bool, in_data: bool) -> Result<Pend, Stop> {
        let pend = match target {
            Target::Function(key) | Target::Direct(key) => match self.slots.get(key) {
                Some(&code) if in_data => {
                    self.taken.push(code);
                    Pend::Pointer(code)
                }
                Some(&code) => Pend::Code(code),
                // alloc: error path
                None => return Err(Stop(format!("names function {key:?}, which the build of its crate did not reach"))),
            },
            Target::Static { item, instance } => match self.statics.get(&(*item, *instance)) {
                Some(&n) => Pend::Static(n),
                // alloc: error path
                None => return Err(Stop(format!("names a static ({item:?}) the build of its crate did not reach"))),
            },
            Target::Data(id) => Pend::Data(*id),
            Target::Position(span) => {
                self.positions.push(*span);
                Pend::Position(*span)
            }
            Target::Extern(symbol) | Target::Weak(symbol) => {
                let weak = matches!(target, Target::Weak(_));
                match self.external(symbol, weak, used, called)? {
                    Pend::Code(code) if in_data => {
                        self.taken.push(code);
                        Pend::Pointer(code)
                    }
                    other => other,
                }
            }
            // alloc: error path
            other => return Err(Stop(format!("names {other:?}, which code that counts nothing never names"))),
        };
        Ok(pend)
    }

    /// The relocations of data, every one an 8-byte absolute address.
    fn data_relocs(&mut self, relocs: &Relocs, what: &str) -> Result<Vec<(u32, Pend, i64)>, Stop> {
        let mut out = Vec::with_capacity(relocs.len()); // alloc: once per blob, per crate compiled
        for reloc in relocs.iter() {
            if reloc.kind != RelocKind::Abs64 {
                // alloc: error path
                return Err(Stop(format!("has a {:?} relocation in {what}, where only 8-byte addresses are", reloc.kind)));
            }
            let pend = self.target(reloc.target, Use::Address, false, true)?;
            out.push((reloc.at, pend, reloc.addend));
        }
        Ok(out)
    }

    /// The blob of a function's or a static's own data, once per content.
    fn put_data(&mut self, data: &Data) -> Result<(), Stop> {
        if self.data.contains_key(&data.id) {
            return Ok(());
        }
        let relocs = self.data_relocs(&data.relocs, "a constant")?;
        let n = self.blobs.len() as u32;
        // clone: once per constant, per crate compiled, into the image's bytes
        self.blobs.push(PendBlob { align: data.align, init: Init::Bytes(data.bytes.clone()), writable: data.writable, relocs });
        self.data.insert(data.id, n);
        Ok(())
    }
}

/// The names `func` has, by position, from their user name: what a relocation of its code
/// names.
fn name_positions(func: &ir::Function) -> HashMap<(u32, u32), u32> {
    func.params
        .user_named_funcs()
        .values()
        .zip(0u32..)
        .map(|(it, at)| ((it.namespace, it.index), at))
        .collect() // alloc: once per function compiled
}

/// Cranelift's library routines stencil's lowering calls, by the C library's name: rounding
/// and fused multiply-add on CPUs without them, each a function of scalars.
fn libcall_symbol(call: ir::LibCall) -> Option<&'static str> {
    Some(match call {
        ir::LibCall::CeilF32 => "ceilf",
        ir::LibCall::CeilF64 => "ceil",
        ir::LibCall::FloorF32 => "floorf",
        ir::LibCall::FloorF64 => "floor",
        ir::LibCall::TruncF32 => "truncf",
        ir::LibCall::TruncF64 => "trunc",
        ir::LibCall::NearestF32 => "nearbyintf",
        ir::LibCall::NearestF64 => "nearbyint",
        ir::LibCall::FmaF32 => "fmaf",
        ir::LibCall::FmaF64 => "fma",
        _ => return None,
    })
}

/// The names of the files of `krate` and every crate it depends on, as the evaluator names
/// them (`file!()`, `Location`).
fn file_names(db: &Db, krate: Crate) -> HashMap<FileId, String> {
    let mut names = HashMap::default();
    for &each in krate.transitive_deps(db) {
        let root = each.root_file_id(db).file_id(db);
        let source_root = db.source_root(db.file_source_root(root).source_root_id(db)).source_root_ref(db);
        for file in source_root.iter() {
            if names.contains_key(&file) {
                continue;
            }
            if let Some(name) = hir_ty_mir::mir::location::source_path(db, file) {
                names.insert(file, name.to_owned()); // alloc: once per file, per crate compiled
            }
        }
    }
    names
}

fn hashes(names: &Arc<HashMap<FileId, String>>) -> HashSource {
    let names = Arc::clone(names);
    // alloc: once per crate compiled, the files' names
    HashSource::Items(ItemHashes::new(Box::new(move |file: FileId| names.get(&file).cloned()))) // clone: a file's name, once per position
}

/// The build's policy: static code of the host, whose units are made the tier's by
/// [`tier_spec`], with the macro crate's own `-C overflow-checks` and `-C debug-assertions`
/// (its cfg, as its profile set them): what the code that inherits its checks is built with.
fn policy(overflow_checks: bool, debug_assertions: bool) -> Result<(Policy, blaze_stencil::Target), Stop> {
    let stop = |what: String| Stop(format!("can't be compiled: {what}")); // alloc: error path
    let target = hir_ty_codegen::Target::selected().map_err(stop)?;
    let Some(mut stencils) = target.stencil() else {
        // alloc: error path
        return Err(stop(format!("blaze has no copy-and-patch stencils for {}", target.triple())));
    };
    stencils.pic = false;
    let library = blaze_stencil::library_id(&stencils).map_err(|e| stop(e.to_string()))?; // alloc: error path
    let mut toolchain = hir_ty_codegen::v2::toolchain(hir_ty_codegen::Target::host(), &[], Runs::Here).map_err(stop)?;
    hir_ty_codegen::v2::select(&toolchain).map_err(stop)?;
    toolchain.relocation = Relocation::Static;
    toolchain.stencils = Some(blaze_format::hex::encode(&library)); // alloc: once per crate compiled
    let policy = Policy {
        toolchain,
        profile: Profile {
            opt_level: OptLevel::O0,
            overflow_checks,
            debug_assertions,
            debuginfo: DebugInfo::None,
            panic: Panic::Unwind,
            packages: Default::default(),
            build_override: Override::default(),
        },
        mode: Mode::Output,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
    };
    Ok((policy, stencils))
}

/// `spec` as the tier compiles it: copy-and-patch's IR of static code that counts nothing
/// and folds nothing, everything else (overflow checks, debug assertions) as the build chose.
/// A unit planned above opt-level 0 is refused naming `what`: the tier compiles only
/// copy-and-patch code, never another backend's plan. Std is planned at its shipped
/// opt-level 3 until a policy can plan it at the profile's (codegen, queue 01466).
fn tier_spec(mut spec: Spec, what: &str) -> Result<Spec, Stop> {
    if let Backend::Llvm { opt } = spec.backend {
        // alloc: error path
        return Err(Stop(format!(
            "can't compile `{what}`: it is planned at opt-level {opt}, and the compiled tier compiles only \
             copy-and-patch code at opt-level 0 (planning std at opt-level 0 waits on codegen queue 01466)"
        )));
    }
    spec.backend = Backend::Stencil;
    spec.counted = false;
    spec.fold = false;
    spec.debuginfo = DebugInfo::None;
    spec.pgo = Pgo::None;
    spec.toolchain.relocation = Relocation::Static;
    Ok(spec)
}

/// The width of each parameter of the confined `sig` but `vmctx`, and of its result.
fn widths(sig: &ir::Signature) -> Result<(Vec<u8>, u8), Stop> {
    let params = &sig.params[..sig.params.len().saturating_sub(1)];
    // alloc: once per stub, per crate compiled
    let widths: Vec<u8> = params.iter().map(|it| it.value_type.bytes() as u8).collect();
    let ret = match sig.returns.as_slice() {
        [] => 0,
        [one] => one.value_type.bytes() as u8,
        // alloc: error path
        more => return Err(Stop(format!("calls a shim returning {} values, past the one a shim gives", more.len()))),
    };
    Ok((widths, ret))
}

/// The bridge's `Closure::call` as its stub is called: `(sret, &mut self, &buffer, vmctx)` in
/// some order, the buffer passed by its address, as Rust passes a value of five words.
fn bridge_abi(sig: &ir::Signature) -> Result<BridgeAbi, Stop> {
    let refuse = |what: &str| Stop(format!("calls the bridge's `Closure::call` {what}, which the compiled tier does not pass")); // alloc: error path
    let params = &sig.params[..sig.params.len().saturating_sub(1)];
    let ret = params.iter().position(|it| it.purpose == ir::ArgumentPurpose::StructReturn);
    let mut normal = params.iter().enumerate().filter(|(_, it)| it.purpose == ir::ArgumentPurpose::Normal);
    let (Some(ret), Some(_this), Some((buffer, _)), None) = (ret, normal.next(), normal.next(), normal.next()) else {
        return Err(refuse("with its values in other places than its result's address, `self` and the buffer's"));
    };
    if params.iter().any(|it| matches!(it.purpose, ir::ArgumentPurpose::StructArgument(_))) {
        return Err(refuse("with its buffer on the machine's stack"));
    }
    Ok(BridgeAbi { ret, buffer })
}

fn compile(db: &Db, krate: Crate, bridge: &Bridge, macros: &[(FunctionId, bool)]) -> Result<Compiled, Stop> {
    let Some(overflow_checks) = hir_ty_mir::mir::overflow::crate_checks_overflow(db, krate) else {
        // alloc: error path
        return Err(Stop("is a crate of the standard library, which has no overflow-checks setting of its own".to_owned()));
    };
    let debug_assertions = krate.cfg_options(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::debug_assertions));
    let (policy, stencils) = policy(overflow_checks, debug_assertions)?;
    let names = Arc::new(file_names(db, krate)); // alloc: once per crate compiled, the files' names
    let build_time = Arc::new(BuildTime(Default::default())); // alloc: once per crate compiled
    let frontend = HirFrontend::new(Databases::new(db), hashes(&names), build_time).with_policy(&policy);
    // The crate's code is its own, and what it calls of its dependencies is asked as its.
    let name = krate.extra_data(db).display_name.as_ref().map_or("proc-macro", |it| it.canonical_name().as_str());
    let origin = Origin::BuildTime(Package::new(name));

    // alloc: once per crate compiled, the roots
    let mut roots: Vec<(HirInstance, Origin)> = Vec::with_capacity(macros.len() + 1);
    for &(function, attr) in macros {
        let expand = if attr { bridge.expand2 } else { bridge.expand1 };
        // alloc: error path
        let root = frontend.root_with_fn_items(expand, &[function]).map_err(|what| Stop(format!("can't be compiled: {what}")))?;
        roots.push((root, origin));
    }
    roots.push((frontend.root(bridge.buffer_new), origin));
    let backends = Backends::default();
    let memo = Memo::new();
    let reached = Build::new(&backends, &frontend, &policy, &memo)
        .reach(&roots, &[])
        .map_err(|errors| errors.first().map_or_else(|| Stop("can't be compiled".to_owned()), codegen_stop))?; // alloc: error path

    let claims = Claims { db, krate, closure_call: bridge.closure_call, bound: Default::default() };
    let mut signatures = Signatures::default();
    // The items the code is of, whose span tables its positions are in.
    let mut items = Vec::with_capacity(reached.functions.len() + reached.statics.len()); // alloc: once per crate compiled
    let mut b = Builder {
        claims: &claims,
        slots: HashMap::default(),
        stubs: Vec::new(), // alloc: none, empty until a name is bound
        stub_of_symbol: HashMap::default(),
        blobs: Vec::new(), // alloc: none, empty until a blob is made
        data: HashMap::default(),
        statics: HashMap::default(),
        positions: Vec::new(), // alloc: none, empty until a position is named
        by_sig: HashMap::default(),
        taken: Vec::new(), // alloc: none, empty until an address is taken
    };

    // Each reached function is compiled, or a stub when a shim claims it.
    let mut compiled_of: Vec<Option<u32>> = Vec::with_capacity(reached.functions.len()); // alloc: once per crate compiled
    let mut count = 0u32;
    for (instance, unit) in &reached.functions {
        let claimed = instance.function().and_then(|func| bind::Shims::claimed(&claims, func));
        let code = match claimed {
            Some(id) => {
                let bound = std::mem::replace(&mut claims.bound.borrow_mut()[usize::from(id)], Bound::Unshimmed(String::new())); // alloc: none, an empty name
                let symbol = frontend.describe(instance, &origin).map_err(|e| codegen_stop(&e))?.name;
                compiled_of.push(None);
                b.stub(&symbol, bound)
            }
            None => {
                compiled_of.push(Some(count));
                count += 1;
                Code::Compiled(count - 1)
            }
        };
        b.slots.insert(unit.instance.key(), code);
    }

    // The statics, before the code that names them.
    let static_spec = tier_spec(policy.choose(&origin, None).map_err(|why| Stop(format!("can't be compiled: {why}")))?.first, "its statics")?; // alloc: error path
    // alloc: once per crate compiled
    let mut statics = Vec::with_capacity(reached.statics.len());
    let mut static_values = Vec::with_capacity(reached.statics.len()); // alloc: once per crate compiled
    for (value, refs) in &reached.statics {
        let value: &HirStatic = value;
        let compiled = backends.compile_static(&frontend, &static_spec, value, refs).map_err(|e| codegen_stop(&e))?;
        let n = b.blobs.len() as u32;
        b.statics.insert((compiled.item, compiled.instance), n);
        items.push(compiled.item);
        // A placeholder until its relocations are made, which may name statics after it.
        b.blobs.push(PendBlob { align: 1, init: Init::Zeroed(0), writable: false, relocs: Vec::new() }); // alloc: none, an empty placeholder
        static_values.push((n, compiled));
        statics.push(value.0);
    }
    let mut static_table = Vec::with_capacity(statics.len()); // alloc: once per crate compiled
    for ((n, value), id) in static_values.into_iter().zip(statics) {
        for data in &value.data {
            b.put_data(data)?;
        }
        let writable = value.mutable || value.thread_local;
        let (init, relocs, len) = match value.init {
            StaticInit::Bytes { bytes, relocs } => {
                let len = bytes.len() as u32;
                (Init::Bytes(bytes), b.data_relocs(&relocs, "a static")?, len)
            }
            StaticInit::Zeroed { size } => {
                // alloc: error path
                let size = u32::try_from(size).map_err(|_| Stop(format!("has a static of {size} bytes, past 4 GiB")))?;
                (Init::Zeroed(size), Vec::new(), size) // alloc: none, no relocations
            }
        };
        b.blobs[n as usize] = PendBlob { align: value.align, init, writable, relocs };
        static_table.push((id, n, len));
    }

    // The functions: lowered, confined and compiled.
    let mut functions: Vec<Compiledfn> = Vec::with_capacity(count as usize); // alloc: once per crate compiled
    let mut sigs: Vec<Sig> = Vec::with_capacity(count as usize); // alloc: once per crate compiled
    let mut bridge_abi_of: Option<BridgeAbi> = None;
    for (((instance, unit), refs), compiled) in reached.functions.iter().zip(&reached.refs).zip(&compiled_of) {
        let Some(n) = *compiled else { continue };
        items.push(unit.item);
        let described = frontend.describe(instance, &origin).map_err(|e| codegen_stop(&e))?;
        let spec = tier_spec(
            policy
                .choose(&described.origin, described.overflow_checks)
                .map_err(|why| Stop(format!("can't compile `{}`: {why}", described.name)))? // alloc: error path
                .first,
            &described.name,
        )?;
        let unit = Unit {
            instance: blaze_codegen::Instance { settings: spec.settings(), ..unit.instance },
            backend: Backend::Stencil,
            ..unit.clone() // clone: plain IDs, the unit as the tier's spec makes it
        };
        let name = described.name;
        let lowering = lowered(&frontend, &unit, &spec, instance, refs).map_err(|e| codegen_stop(&e))?;
        let (mut func, targets, data) = match lowering {
            Lowering::Ir { func, names, data } => (func, names, data),
            Lowering::Asm(_) => return Err(Refusal { name, why: Why::Assembly }.stop()),
            // The tier binds Rust's allocator by its symbol (`bind::instance`); a build rooted
            // at the clients names it as an external and never reaches the shim's functions.
            Lowering::Allocator(symbol) => {
                // alloc: error path
                return Err(Stop(format!("reached the allocator shim's `{symbol}` as a function to compile")));
            }
        };
        for it in &data {
            b.put_data(it)?;
        }
        // Each name's target, decided as confine asks.
        // alloc: once per function compiled, one per name
        let mut resolved: Vec<Option<Pend>> = vec![None; targets.len()];
        let mut called: Vec<bool> = vec![false; targets.len()]; // alloc: as above
        let confined = confine::confine(&mut func, &name, &mut signatures, |i, used| {
            let Some(target) = targets.get(i as usize) else {
                return Err(Stop(format!("has a name {i} past its {} in `{name}`", targets.len()))); // alloc: error path
            };
            if used == Use::Call {
                called[i as usize] = true;
            }
            let pend = b.target(target, used, called[i as usize], false)?;
            resolved[i as usize] = Some(pend);
            Ok(match pend {
                Pend::Code(_) => Name::Code,
                _ => Name::Data,
            })
        })?;
        b.by_sig.entry(confined.sig).or_insert_with(|| func.signature.clone()); // clone: once per signature, per crate compiled
        sigs.push(confined.sig);

        // The signature each stub is called with, from its callers.
        let by_position = name_positions(&func);
        for ext in func.dfg.ext_funcs.values() {
            let ir::ExternalName::User(user) = ext.name else { continue };
            let user = &func.params.user_named_funcs()[user];
            let Some(&at) = by_position.get(&(user.namespace, user.index)) else { continue };
            let Some(Ref::Given(given)) = confined.refs.get(at as usize).copied() else { continue };
            let Some(Some(Pend::Code(Code::Stub(s)))) = resolved.get(given as usize).copied() else { continue };
            let sig = &func.dfg.signatures[ext.signature];
            let plan = &mut b.stubs[s as usize];
            match &plan.sig {
                None => plan.sig = Some(sig.clone()), // clone: once per stub, per crate compiled
                Some(it) if it == sig => {}
                Some(_) => {
                    // alloc: error path
                    return Err(Stop(format!("calls `{}` with two signatures, in `{name}` and before", plan.name)));
                }
            }
            if matches!(plan.bound, Bound::Bridge) && bridge_abi_of.is_none() {
                bridge_abi_of = Some(bridge_abi(sig)?);
            }
        }

        let code = blaze_stencil::compile(&func, &stencils)
            // alloc: error path
            .map_err(|e| Stop(format!("can't compile `{name}` by copy-and-patch: {e}")))?;
        let mut relocs = Vec::with_capacity(code.relocs.len()); // alloc: once per function compiled
        for reloc in &code.relocs {
            let pend = match reloc.name {
                blaze_stencil::code::Name::User { namespace, index } => {
                    let at = by_position.get(&(namespace, index)).copied();
                    match at.and_then(|at| confined.refs.get(at as usize).copied()) {
                        Some(Ref::Given(given)) => match resolved.get(given as usize).copied().flatten() {
                            Some(pend) => pend,
                            // alloc: error path
                            None => return Err(Stop(format!("has `{name}` naming {given}, which confine bound to nothing"))),
                        },
                        Some(Ref::Pointer(given)) => match resolved.get(given as usize).copied().flatten() {
                            Some(Pend::Code(code)) => {
                                b.taken.push(code);
                                Pend::Pointer(code)
                            }
                            // alloc: error path
                            _ => return Err(Stop(format!("takes in `{name}` the pointer of {given}, which is no code"))),
                        },
                        Some(Ref::Table) => Pend::Table,
                        Some(Ref::TableSize) => Pend::TableSize,
                        // alloc: error path
                        None => return Err(Stop(format!("has `{name}` relocated at a name it does not declare"))),
                    }
                }
                blaze_stencil::code::Name::LibCall(call) => match libcall_symbol(call) {
                    Some(symbol) => b.external(symbol, false, Use::Call, true)?,
                    // alloc: error path
                    None => return Err(Stop(format!("has `{name}` calling Cranelift's {call:?}, which the compiled tier has no shim for"))),
                },
                blaze_stencil::code::Name::Own(at) => {
                    relocs.push((reloc.at, reloc.kind, Pend::Code(Code::Compiled(n)), reloc.addend + i64::from(at)));
                    continue;
                }
                blaze_stencil::code::Name::Known(symbol) => {
                    // alloc: error path
                    return Err(Stop(format!("has `{name}` using {symbol:?}, which confined code can't reach")));
                }
                blaze_stencil::code::Name::Counter => {
                    // alloc: error path
                    return Err(Stop(format!("has `{name}` counting its calls, which the tier's code never does")));
                }
            };
            relocs.push((reloc.at, reloc.kind, pend, reloc.addend));
        }
        functions.push(Compiledfn { name, code, relocs });
    }

    // The `Location` of each position named, and its file's name.
    let positions = locations(db, &hashes(&names), &items, &mut b)?;

    // The stubs, numbered after the compiled functions; then the thunks and `resume`.
    let conv = host_conv();
    let compiled_count = functions.len() as u32;
    let index = |code: Code| match code {
        Code::Compiled(n) => n,
        Code::Stub(n) => compiled_count + n,
    };
    let mut stubs: Vec<Stub> = Vec::with_capacity(b.stubs.len()); // alloc: once per crate compiled
    let mut stub_functions: Vec<Function> = Vec::with_capacity(b.stubs.len()); // alloc: once per crate compiled
    let mut stub_sigs: Vec<Sig> = Vec::with_capacity(b.stubs.len()); // alloc: once per crate compiled
    for (plan, id) in std::mem::take(&mut b.stubs).into_iter().zip(0u32..) {
        let sig = plan.sig.unwrap_or_else(|| {
            let mut sig = ir::Signature::new(conv);
            sig.params.push(ir::AbiParam::special(ir::types::I64, ir::ArgumentPurpose::VMContext));
            sig
        });
        let (params, ret) = widths(&sig)?;
        let func = glue::stub(id, &sig, conv).map_err(|what| Stop(format!("can't make the stub of `{}`: {what}", plan.name)))?; // alloc: error path
        let code = blaze_stencil::compile(&func, &stencils)
            .map_err(|e| Stop(format!("can't compile the stub of `{}`: {e}", plan.name)))?; // alloc: error path
        let table_sig = confine::intern(&mut signatures, &sig);
        b.by_sig.entry(table_sig).or_insert_with(|| sig.clone()); // clone: once per stub's signature
        stub_sigs.push(table_sig);
        stub_functions.push(glue_function(&plan.name, code, machine::dispatch_address())?);
        stubs.push(Stub { bound: plan.bound, params, ret });
    }

    // Which functions have table entries.
    let total = compiled_count as usize + stub_functions.len();
    let mut entry: Vec<bool> = vec![false; total]; // alloc: once per crate compiled
    for &code in &b.taken {
        entry[index(code) as usize] = true;
    }
    let mut all_sigs: Vec<Sig> = Vec::with_capacity(total); // alloc: once per crate compiled
    all_sigs.extend_from_slice(&sigs);
    all_sigs.extend_from_slice(&stub_sigs);

    // A thunk for each signature with an entry, and for each root.
    let client_index: Vec<(FunctionId, u32)> = macros
        .iter()
        .zip(&roots)
        .map(|(&(function, _), (root, _))| {
            let at = reached.functions.iter().position(|(it, _)| it == root);
            match at.and_then(|at| compiled_of[at]) {
                Some(n) => Ok((function, n)),
                // alloc: error path
                None => Err(Stop("reached no code of a macro's client".to_owned())),
            }
        })
        .collect::<Result<_, _>>()?; // alloc: once per crate compiled
    let buffer_root = &roots.last().expect("`Buffer::new` is a root").0;
    let buffer_new = reached
        .functions
        .iter()
        .position(|(it, _)| it == buffer_root)
        .and_then(|at| compiled_of[at])
        .ok_or_else(|| Stop("reached no code of the bridge's `Buffer::new`".to_owned()))?; // alloc: error path
    // alloc: once per crate compiled
    let mut rooted: Vec<Sig> = client_index.iter().map(|&(_, n)| sigs[n as usize]).collect();
    rooted.push(sigs[buffer_new as usize]);
    let mut entered: Vec<(Sig, bool)> = (0..total).filter(|&n| entry[n]).map(|n| (all_sigs[n], false)).collect(); // alloc: once per crate compiled
    entered.extend(rooted.iter().map(|&sig| (sig, true)));
    entered.sort_unstable_by_key(|it| (it.0.0, !it.1));
    entered.dedup_by_key(|it| it.0);
    let mut thunk_functions: Vec<Function> = Vec::with_capacity(entered.len()); // alloc: once per crate compiled
    let mut thunk_plans: Vec<(Sig, u32, u32, u32)> = Vec::with_capacity(entered.len()); // alloc: once per crate compiled
    for &(sig, root) in &entered {
        let target = &b.by_sig[&sig];
        // A pointer of a signature no thunk can enter is entered by no one: the run calls
        // through the table only `run`, `reserve` and `drop`, and errs naming the signature
        // when one has no thunk.
        let func = match glue::thunk(target, conv) {
            Ok(func) => func,
            Err(_) if !root => continue,
            Err(what) => return Err(Stop(format!("can't make an entry of {target}: {what}"))), // alloc: error path
        };
        let code = blaze_stencil::compile(&func, &stencils).map_err(|e| Stop(format!("can't compile an entry: {e}")))?; // alloc: error path
        let calls = Call::of(&code);
        let Some(&Call { ret, landing: Some(landing) }) = calls.first() else {
            return Err(Stop("made an entry with no call that lands its escapes".to_owned())); // alloc: error path
        };
        thunk_plans.push((sig, ret, landing, code.frame));
        thunk_functions.push(glue_function("the compiled tier's entry", code, machine::dispatch_address())?);
    }

    // Every blob, its targets numbered.
    let to = |pend: Pend, b: &Builder<'_>| -> Result<To, Stop> {
        Ok(match pend {
            Pend::Code(code) => To::Code(index(code)),
            Pend::Pointer(code) => To::Pointer(index(code)),
            Pend::Static(n) | Pend::Blob(n) => To::Blob(n),
            Pend::Data(id) => match b.data.get(&id) {
                Some(&n) => To::Blob(n),
                None => return Err(Stop(format!("names a constant {id:?} no function or static has"))), // alloc: error path
            },
            Pend::Position(span) => match positions.get(&span) {
                Some(&n) => To::Blob(n),
                None => return Err(Stop(format!("names a position {span:?} no span table has"))), // alloc: error path
            },
            Pend::Guest(at) => To::Guest(at),
            Pend::Host(address) => To::Host(address),
            Pend::Table => To::Table,
            Pend::TableSize => To::TableSize,
        })
    };
    let mut blobs = Vec::with_capacity(b.blobs.len()); // alloc: once per crate compiled
    for blob in std::mem::take(&mut b.blobs) {
        let mut relocs = Vec::with_capacity(blob.relocs.len()); // alloc: once per blob
        for (at, pend, addend) in blob.relocs {
            relocs.push((at, to(pend, &b)?, addend));
        }
        blobs.push(Blob { align: blob.align, init: blob.init, writable: blob.writable, relocs });
    }

    let mut placed: Vec<Function> = Vec::with_capacity(total + thunk_functions.len() + 1); // alloc: once per crate compiled
    for (function, n) in functions.into_iter().zip(0usize..) {
        let mut relocs = Vec::with_capacity(function.relocs.len()); // alloc: once per function placed
        for (at, kind, pend, addend) in function.relocs {
            relocs.push(CodeReloc { at, kind, to: to(pend, &b)?, addend });
        }
        let calls = Call::of(&function.code);
        placed.push(Function {
            name: function.name,
            align: function.code.align,
            relocs,
            entry: entry[n].then_some(sigs[n]),
            frame: function.code.frame,
            calls,
            traps: function.code.traps,
            bytes: function.code.bytes,
        });
    }
    for (mut function, n) in stub_functions.into_iter().zip(compiled_count as usize..) {
        function.entry = entry[n].then_some(all_sigs[n]);
        placed.push(function);
    }
    let first_thunk = placed.len() as u32;
    placed.extend(thunk_functions);
    let resume = placed.len() as u32;
    placed.push(Function {
        name: "the compiled tier's resume".to_owned(), // alloc: once per crate compiled
        bytes: machine::resume_code().to_vec(),        // alloc: once per crate compiled
        align: 16,
        relocs: Vec::new(), // alloc: none, no relocations
        entry: None,
        frame: 0,
        calls: Vec::new(), // alloc: none, no calls
        traps: Vec::new(), // alloc: none, no traps
    });

    let image = Image::load(Input { functions: placed, blobs, bounds: Bounds::DEFAULT })?;
    let thunks = thunk_plans
        .into_iter()
        .zip(first_thunk..)
        .map(|((sig, ret, landing, frame), n)| {
            let entry = image.code(n).expect("every thunk is placed");
            (sig, Thunk { entry, ret: entry + u64::from(ret), landing: entry + u64::from(landing), frame })
        })
        .collect(); // alloc: once per crate compiled
    let resume = image.code(resume).ok_or_else(|| Stop("placed no `resume`".to_owned()))?; // alloc: error path
    Ok(Compiled {
        image,
        stubs,
        clients: client_index,
        buffer_new,
        thunks,
        sigs: all_sigs,
        signatures: b.by_sig,
        statics: static_table,
        bridge: bridge_abi_of,
        resume,
    })
}

/// A glue function placed as it was compiled, its one name the dispatcher at `dispatch`.
fn glue_function(name: &str, code: blaze_stencil::Code, dispatch: u64) -> Result<Function, Stop> {
    let mut relocs = Vec::with_capacity(code.relocs.len()); // alloc: once per glue function
    for reloc in &code.relocs {
        match reloc.name {
            blaze_stencil::code::Name::User { namespace: glue::NAMESPACE, index: glue::DISPATCH } => {
                relocs.push(CodeReloc { at: reloc.at, kind: reloc.kind, to: To::Host(dispatch), addend: reloc.addend });
            }
            // alloc: error path
            other => return Err(Stop(format!("has {name} naming {other:?}, which glue never names"))),
        }
    }
    let calls = Call::of(&code);
    Ok(Function {
        name: name.to_owned(), // alloc: once per glue function
        align: code.align,
        relocs,
        entry: None,
        frame: code.frame,
        calls,
        traps: code.traps,
        bytes: code.bytes,
    })
}

/// The `Location` record of each position the code names, each pointing at its file's name:
/// read-only blobs, by the position.
fn locations(
    db: &Db,
    hashes: &HashSource,
    items: &[blaze_items::ItemKey],
    b: &mut Builder<'_>,
) -> Result<HashMap<SpanRef, u32>, Stop> {
    let mut out = HashMap::default();
    if b.positions.is_empty() {
        return Ok(out);
    }
    b.positions.sort_unstable();
    b.positions.dedup();
    let anchors: Vec<ContentId> = {
        let mut it: Vec<ContentId> = b.positions.iter().map(|it| it.anchor).collect(); // alloc: once per crate compiled
        it.sort_unstable();
        it.dedup();
        it
    };
    // alloc: error path
    let tables = span_tables_of(db, hashes, items, anchors).map_err(|what| Stop(format!("can't place its positions: {what}")))?;
    let mut files: HashMap<String, u32> = HashMap::default();
    for (anchor, bytes) in tables {
        // alloc: error path
        let table = blaze_link::SpanTable::decode(&bytes).map_err(|what| Stop(format!("has a span table it can't read: {what}")))?;
        let start = b.positions.partition_point(|it| it.anchor < anchor);
        let end = b.positions.partition_point(|it| it.anchor <= anchor);
        for at in start..end {
            let span = b.positions[at];
            let Some(&position) = table.spans.get(span.index as usize) else {
                // alloc: error path
                return Err(Stop(format!("names span {} of a table of {}", span.index, table.spans.len())));
            };
            let Some(file) = table.files.get(position.file as usize) else {
                // alloc: error path
                return Err(Stop(format!("names file {} of a span table of {}", position.file, table.files.len())));
            };
            let name = match files.get(file.as_str()) {
                Some(&n) => n,
                None => {
                    let n = b.blobs.len() as u32;
                    let bytes = blaze_link::file_name(file);
                    b.blobs.push(PendBlob { align: 1, init: Init::Bytes(bytes), writable: false, relocs: Vec::new() }); // alloc: none, no relocations
                    files.insert(file.clone(), n); // clone: once per file named, per crate compiled
                    n
                }
            };
            let n = b.blobs.len() as u32;
            let record = blaze_link::location(file.len(), position);
            b.blobs.push(PendBlob {
                align: blaze_link::LOCATION_ALIGN as u32,
                init: Init::Bytes(record.to_vec()), // alloc: once per position, per crate compiled
                writable: false,
                relocs: vec![(0, Pend::Blob(name), 0)], // alloc: as above
            });
            out.insert(span, n);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
