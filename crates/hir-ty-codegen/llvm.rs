//! LLVM as the producer of a function's machine code.
//!
//! 1. [`lower`] turns the Cranelift IR of the function into an LLVM function of a module of
//!    its own. What the function refers to is declared there under the names of [`Symbol`];
//!    the bodies of callees LLVM is shown are functions beside it ([`shown_name`]), and on
//!    Mach-O the function is built into a [`wrap`]per.
//! 2. LLVM optimizes the module, if asked to, the bodies shown give way to the functions
//!    they are of ([`unshow`]), and LLVM writes an object file into memory.
//! 3. [`emitted`] reads the object back: the code, the constants LLVM put beside it, the
//!    relocations, with their [`Symbol`]s turned back into requests, and the unwind table.
//!
//! Anything on the way that this has no answer for is an [`Unhandled`], and the build stops
//! on it ([`super::CodegenError::LlvmRefused`]).
//!
//! # Unsafe
//!
//! This module is the crate's one door to LLVM's C API (`vendor/llvm-sys`, whose root states
//! the contract: who owns each handle, which strings need a NUL, the thread rules). Every
//! call into it is here or in two children, each behind a safe function:
//! - this file: the passes kept per target machine ([`Kept`]) and the report they keep
//!   ([`report`]), the diagnostic handler ([`diagnosed`]), the process's command-line options, the host CPU, the byte GEPs the
//!   lowering builds ([`byte_offset`]), and the handful of inkwell calls that are `unsafe`
//!   (deleting a function nothing uses, wrapping a raw call site);
//! - [`rebase`] and [`release`]: walks of a module's raw IR (use lists, constant expressions,
//!   call operands) that inkwell has no type for. Through inkwell each value read would also be
//!   classified into its enum, one or two more calls into LLVM per operand.
//!
//! The safe version of all of it is LLVM's own `LLVMRunPasses` and inkwell's safe surface,
//! which rebuild the pass builder, its analysis managers and a process-wide lock on every run
//! ([`Kept`]), and cannot reach what `rebase` and `release` read. `lower`, `pgo`, `report`,
//! `sanitize` and `emitted` forbid unsafe code. The values are of one context, the thread's
//! ([`Machine`]): no handle crosses threads.

use blaze_hash::hash_map::Entry;
use std::{cell::RefCell, fmt, sync::Once};

use blaze_hash::{HashMap, HashSet};
use cranelift_codegen::ir::{self, Opcode, Type};
use inkwell::{
    AddressSpace, GlobalVisibility, OptimizationLevel,
    attributes::{Attribute, AttributeLoc},
    builder::{Builder, BuilderError},
    context::Context,
    llvm_sys::{
        support::LLVMParseCommandLineOptions,
        transforms::pass_builder::{
            LLVMBlazeCreatePasses, LLVMBlazeDisposePasses, LLVMBlazeHasCpu, LLVMBlazeHasFeature,
            LLVMBlazeOpaquePasses, LLVMBlazeRunPasses,
        },
    },
    memory_buffer::MemoryBuffer,
    module::{Linkage, Module},
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
    },
    types::{AnyType, BasicTypeEnum},
    values::{BasicValueEnum, FunctionValue, GlobalValue, IntValue, PointerValue},
};
use smallvec::SmallVec;
use std::alloc::Global;

use super::{
    Arch,
    backend::{Optimization, Passes, Phase, Selection, Tally},
    fragment::{Code, Hint, Wants},
    imports::Import,
};

mod emitted;
mod lower;
pub(crate) mod pgo;
mod rebase;
pub(crate) mod release;
pub(crate) mod report;
pub(crate) mod sanitize;

/// What runs again where [`rebase::rebase`] said what addresses are based on: what is stored
/// through one and loaded again is forwarded, and a slot that is then only written goes.
const REBASED: &str = "function(instcombine<no-verify-fixpoint>,sroa,gvn,dse,\
                       instcombine<no-verify-fixpoint>,simplifycfg)";

use lower::{Defined, Lowering};

/// The name of the function being compiled, in the module and in the object.
const OWN: &str = "blaze.own";
const PERSONALITY: &str = "rust_eh_personality";

/// Whether every call of `func` that unwinds has one handler, which takes the exception alone.
/// A call of a body Cranelift built in has handlers of its own and of the caller's, which
/// may take the caller's values; LLVM lowers those too, each with a landing pad of its own.
pub(super) fn plain_handlers(func: &ir::Function) -> bool {
    let dfg = &func.dfg;
    dfg.exception_tables.values().all(|table| {
        let mut items = table.items();
        match (items.next(), items.next()) {
            (Some(ir::ExceptionTableItem::Tag(_, handler)), None) => {
                let mut args = handler.args(&dfg.value_lists);
                matches!((args.next(), args.next()), (Some(ir::BlockArg::TryCallExn(0)), None))
            }
            _ => false,
        }
    })
}

/// A function of a unit LLVM compiles in one module ([`emit_unit`]).
pub(super) struct Member<'a> {
    pub(super) func: &'a ir::Function,
    /// The slots of its locals with a name.
    pub(super) named: &'a [ir::StackSlot],
    /// What its parameters point to, as [`Wants::pointees`].
    pub(super) pointees: &'a [(u32, super::fragment::Pointee)],
    /// The request the others call it by, which is its name in the module: a call of it from
    /// another is a call of a function of the module, which LLVM may build in.
    pub(super) request: u32,
}

/// `named` are the slots of the locals with a name. `weights`, from what code of the function
/// counted while it ran, become its `!prof`. `imports` are the bodies of what it calls that
/// LLVM may build in ([`super::imports`]).
pub(super) fn emit(
    recorder: &mut Wants,
    named: &[ir::StackSlot],
    func: &ir::Function,
    optimization: Optimization,
    weights: Option<&super::tier::Weights>,
    imports: &[Import],
) -> Result<Code, Unhandled> {
    let pointees = std::mem::take(&mut recorder.pointees);
    let alone = Alone { func, named, pointees: &pointees, weights };
    let emitted = compiled(recorder, &Members::Alone(alone), optimization, imports);
    recorder.pointees = pointees;
    emitted?.members.pop().ok_or(Unhandled::Shape("an object without the function"))?
}

/// What the module of a unit gave ([`emit_unit`]).
pub(super) struct Unit {
    /// The code of each member, or what is wrong with it, which is that function's alone.
    pub(super) members: Vec<Result<Code, Unhandled>>,
    /// The code of each import LLVM left a call of, by the request it is of.
    pub(super) kept: Vec<(u32, Result<Code, Unhandled>)>,
}

/// The functions of a unit, compiled in one module with what each imports ([`super::units`]).
/// What is wrong with the module is every member's.
///
/// An import of a unit is `linkonce_odr` where a compile alone has it `available_externally`:
/// a body LLVM did not build into every call of it is kept, and is the code of the function
/// it is of, which the calls left call. So what a unit's code calls is known before LLVM
/// runs: what its functions call that is not imported, and the same of what is
/// ([`super::units`]).
pub(super) fn emit_unit(
    recorder: &mut Wants,
    members: &[Member<'_>],
    optimization: Optimization,
    imports: &[Import],
) -> Result<Unit, Unhandled> {
    compiled(recorder, &Members::Unit(members), optimization, imports)
}

/// The function of a module of one, which is named [`OWN`] and has nothing else call it.
struct Alone<'a> {
    func: &'a ir::Function,
    named: &'a [ir::StackSlot],
    pointees: &'a [(u32, super::fragment::Pointee)],
    weights: Option<&'a super::tier::Weights>,
}

enum Members<'a> {
    Alone(Alone<'a>),
    Unit(&'a [Member<'a>]),
}

fn compiled(
    recorder: &mut Wants,
    members: &Members<'_>,
    optimization: Optimization,
    imports: &[Import],
) -> Result<Unit, Unhandled> {
    let tally = Tally::global();
    let target = recorder.target;
    let alone = match members {
        Members::Alone(alone) => Some(alone),
        Members::Unit(_) => None,
    };
    let weights = alone.and_then(|it| it.weights);
    // A member's request, none for a function alone.
    let (requested, defined): (SmallVec<[Option<SymbolName>; 1]>, SmallVec<[Defined<'_>; 1]>) =
        match members {
            Members::Alone(it) => {
                let defined = Defined {
                    name: "",
                    func: it.func,
                    pointees: it.pointees,
                    named: it.named,
                    weights,
                    marked: true,
                };
                (SmallVec::from_buf([None]), SmallVec::from_buf([defined]))
            }
            Members::Unit(members) => members
                .iter()
                .map(|it| {
                    let defined = Defined {
                        name: "",
                        func: it.func,
                        pointees: it.pointees,
                        named: it.named,
                        weights: None,
                        marked: true,
                    };
                    (Some(Symbol::Func(it.request).named()), defined)
                })
                .unzip(),
        };
    // LLVM's optimizing register allocator takes more than linear time, as Cranelift's does:
    // one function of dx kept it busy for minutes. Such a function is compiled without
    // optimization, and counted.
    let large = defined.iter().filter(|it| it.func.dfg.num_insts() > super::LARGE).count();
    let optimization = if large > 0 && optimization != Optimization::NONE {
        tally.large.fetch_add(large, std::sync::atomic::Ordering::Relaxed);
        Optimization::NONE
    } else {
        optimization
    };
    let seen = std::mem::take(&mut recorder.seen);
    // Without a line table to keep (see `lower::Marks`), and where a parameter is the only
    // way to its memory.
    let wrapped = alone.is_some()
        && optimization.pipeline().is_some()
        && alone.is_some_and(|it| it.pointees.iter().any(|(_, it)| it.unique))
        && target.object_format == super::ObjectFormat::MachO;
    let names = requested
        .iter()
        .map(|name| match name {
            Some(name) => &**name,
            None if wrapped => INNER,
            None => OWN,
        })
        .collect::<SmallVec<[&str; 1]>>();
    let defined = std::iter::zip(&names, defined)
        .map(|(&name, defined)| Defined { name, ..defined })
        .collect::<SmallVec<[Defined<'_>; 1]>>();
    Machine::with(target, optimization, |context, machine| {
        let module = context.create_module("blaze");
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());
        // alloc: once per module; `Wants` (fragment.rs) lists these, and the lowering looks them up
        let constants: HashMap<u32, &[u8], Global> = recorder.constants().collect();
        // alloc: once per module, as `constants`
        let statics: HashSet<u32, Global> = recorder.statics().collect();
        let resume = recorder.resume();
        let memcmp = recorder.memcmp();
        let blocks_asm = recorder.asm_blocks();
        // alloc: once per module, as `constants`
        let readonly: HashMap<u32, &super::ConstBlob, Global> = recorder.readonly().collect();
        told_constants(
            context,
            &module,
            &readonly,
            machine.get_target_data().get_pointer_byte_size(None) as usize,
        );
        let debug = lower::Debug::of(&module, target, false)?;
        let (shown, failed) = tally.time(Phase::Lower, || {
            // What is wrong with the function of each member, which is then only declared.
            let mut failed = std::iter::repeat_with(|| None::<Unhandled>)
                .take(defined.len())
                .collect::<SmallVec<[_; 1]>>();
            let mut functions = Vec::with_capacity(defined.len());
            for (at, defined) in defined.iter().enumerate() {
                match Lowering::add(
                    context,
                    &module,
                    target,
                    defined,
                    super::backend::V1_FRAME_POINTERS,
                    crate::Features::BASELINE,                ) {
                    Ok(function) => functions.push(Some(function)),
                    Err(e) if alone.is_some() => return Err(e),
                    Err(e) => {
                        failed[at] = Some(e);
                        functions.push(None);
                    }
                }
            }
            // Each function of a unit is a section of its own, which `emitted` reads as the
            // code of that function: by a COMDAT of its own where the format has them. Mach-O
            // has none and keeps the functions apart in `__text` by their symbols
            // (`.subsections_via_symbols`), which is how `emitted` reads them there.
            if alone.is_none() && target.object_format.supports_comdat() {
                for function in functions.iter().flatten() {
                    own_comdat(&module, target, *function)?;
                }
            }
            // Before the functions' bodies: their calls of what is imported are of the bodies.
            let linkage = match alone {
                Some(_) => Linkage::AvailableExternally,
                None => Linkage::LinkOnceODR,
            };
            link(context, &module, imports, linkage)?;
            let mut names = Vec::with_capacity(seen.len());
            names.extend(seen.iter().map(|it| shown_name(it.id.as_u32())));
            let mut shown = Vec::with_capacity(seen.len());
            shown.extend(std::iter::zip(&seen, &names).map(|(it, name)| Defined {
                name: &**name,
                func: &it.func,
                pointees: &it.pointees,
                named: &[],
                weights: None,
                marked: false,
            }));
            // Every body shown is declared before any is defined, as one may call another.
            let mut by_id = HashMap::new_in(Global);
            for (it, defined) in std::iter::zip(&seen, &shown) {
                if let Ok(function) = Lowering::add(
                    context,
                    &module,
                    target,
                    defined,
                    super::backend::V1_FRAME_POINTERS,
                    crate::Features::BASELINE,                ) {
                    mark_hint(context, function, it.hint);
                    by_id.insert(it.id.as_u32(), function);
                }
            }
            // A body shown calls the others shown, as the function being compiled does, so
            // that what LLVM learns of one (that it keeps no pointer it is given, say) holds
            // of those that call it.
            // Each is defined seeing all that were declared: the ones that fail leave `by_id`
            // after.
            let mut unshown: SmallVec<[u32; 4]> = SmallVec::new();
            for (it, defined) in std::iter::zip(&seen, &shown) {
                let Some(&function) = by_id.get(&it.id.as_u32()) else { continue };
                let defined = Lowering::define(
                    context,
                    &module,
                    target,
                    function,
                    defined,
                    &constants,
                    &statics,
                    resume,
                    memcmp,
                    &blocks_asm,
                    &by_id,
                    None,
                );
                if defined.is_err() {
                    unshown.push(it.id.as_u32());
                    unshow(&module, [(it.id.as_u32(), function)]);
                }
            }
            for id in unshown {
                by_id.remove(&id);
            }
            for (at, (defined, function)) in std::iter::zip(&defined, &functions).enumerate() {
                let Some(function) = *function else { continue };
                let lowered = Lowering::define(
                    context,
                    &module,
                    target,
                    function,
                    defined,
                    &constants,
                    &statics,
                    resume,
                    memcmp,
                    &blocks_asm,
                    &by_id,
                    debug.as_ref(),
                );
                match lowered {
                    Ok(()) => {}
                    Err(e) if alone.is_some() => return Err(e),
                    Err(e) => {
                        failed[at] = Some(e);
                        declared_only(&module, function);
                    }
                }
            }
            if let Some(debug) = &debug {
                debug.finish();
            }
            if !imports.is_empty() || alone.is_none() {
                agree_calls(context, &module)?;
            }
            if wrapped && let Some(Some(function)) = functions.first() {
                wrap(context, &module, *function, weights)?;
            }
            Ok::<_, Unhandled>((by_id, failed))
        })?;
        // `BLAZE_DUMP_LLVM=after` prints the IR as the passes left it, anything else as it
        // goes in.
        let dump = std::env::var("BLAZE_DUMP_LLVM").ok();
        if dump.as_deref().is_some_and(|it| it != "after") {
            eprintln!("{}", module.print_to_string().to_string_lossy());
        }
        // LLVM aborts the process on IR it does not accept, so it is checked first.
        tally
            .time(Phase::Verify, || module.verify())
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        if let Some(pipeline) = optimization.pipeline() {
            // rustc vectorizes straight-line code at `-O2` and `-O3`.
            tally.time(Phase::Optimize, || machine.run(&module, pipeline, true))?;
            if rebase::rebase(&module) {
                tally.time(Phase::Optimize, || machine.run(&module, REBASED, false))?;
            }
        }
        // What a compile alone imports is only there to be built in: a body LLVM did not
        // build in is the callee's own code, called. The passes above drop them; a function
        // too large for them gets none. What a unit imports and still calls is kept.
        let mut kept: SmallVec<[(SymbolName, &Import); 4]> = SmallVec::new();
        if !imports.is_empty() {
            let pass = if alone.is_some() { "elim-avail-extern" } else { "globaldce" };
            machine.run(&module, pass, false)?;
        }
        if alone.is_none() {
            for import in imports {
                let name = Symbol::Func(import.id).named();
                let Some(body) = module.get_function(&name) else { continue };
                if body.count_basic_blocks() == 0 {
                    continue;
                }
                body.set_linkage(Linkage::External);
                // A section of its own, as the members' above.
                if target.object_format.supports_comdat() {
                    own_comdat(&module, target, body)?;
                }
                kept.push((name, import));
            }
        }
        unshow(&module, shown.iter().map(|(&id, &body)| (id, body)));
        retold_bytes(context, &module, &readonly);
        if module.get_function(INNER).is_some() {
            return Err(Unhandled::Shape("a body not built into the function that calls it"));
        }
        if dump.as_deref() == Some("after") {
            eprintln!("{}", module.print_to_string().to_string_lossy());
        }
        let object = tally
            .time(Phase::Emit, || machine.write_to_memory_buffer(&module, FileType::Object))
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        // In the object a member alone is `OWN` whether it was wrapped or not.
        let mut read = std::iter::zip(&requested, &defined)
            .map(|(name, defined)| {
                (name.as_ref().map_or(OWN, SymbolName::bare), &defined.func.signature)
            })
            .collect::<SmallVec<[(&str, &ir::Signature); 1]>>();
        read.extend(kept.iter().map(|(name, import)| (name.bare(), &import.export.signature)));
        let mut codes =
            tally.time(Phase::Read, || emitted::read(object.as_slice(), &read, recorder))?;
        let kept_codes = codes.split_off(defined.len());
        let kept = std::iter::zip(kept.iter().map(|(_, import)| import.id), kept_codes);
        Ok(Unit {
            members: std::iter::zip(codes, failed)
                .map(|(code, failed)| match failed {
                    Some(failed) => Err(failed),
                    None => code,
                })
                // alloc: the unit's result, one per member
                .collect(),
            // alloc: the unit's result, one per import kept
            kept: kept.collect(),
        })
    })
}

/// Puts `function` in a COMDAT of its own, of its name, which has LLVM write its code in a
/// section of its own. Refused before LLVM sees it for a format without COMDATs
/// ([`super::ObjectFormat::supports_comdat`]), where LLVM would end the process when it
/// writes the object instead of answering an error.
fn own_comdat<'ctx>(
    module: &Module<'ctx>,
    target: &super::Target,
    function: FunctionValue<'ctx>,
) -> Result<(), Unhandled> {
    let name = function.get_name().to_string_lossy();
    if !target.object_format.supports_comdat() {
        // alloc: the error's message
        return Err(Unhandled::Llvm(format!(
            "a COMDAT for `{name}`, which {:?} objects of {} have no place for",
            target.object_format,
            target.triple(),
        )));
    }
    function.as_global_value().set_comdat(module.get_or_insert_comdat(&name));
    Ok(())
}

/// Leaves `function`, whose body could not be lowered, a declaration of the same name and
/// type, which the other functions of the module call.
fn declared_only<'ctx>(module: &Module<'ctx>, function: FunctionValue<'ctx>) {
    // alloc: a function of a unit that failed to lower; the name outlives its renaming
    let name = function.get_name().to_owned();
    function.as_global_value().set_name("blaze.unlowered");
    let declaration =
        module.add_function(&name.to_string_lossy(), function.get_type(), Some(Linkage::External));
    function.replace_all_uses_with(declaration);
    // SAFETY: `function` is owned by `module`, and `replace_all_uses_with` just above left no
    // use of it; `delete` erases and frees it, and this handle is not used after. A use left
    // behind would point at freed memory.
    unsafe { function.delete() };
}

/// The function being compiled, when it is [`wrap`]ped.
const INNER: &str = "blaze.inner";

/// Has the function being compiled, `inner`, called by one that only calls it, under the
/// name [`OWN`], with the same parameters, and built into that one.
///
/// LLVM builds a function into its caller by telling each access through a `noalias`
/// parameter from those that cannot be through it (`!alias.scope`, `!noalias`): a call of
/// a function that is not passed the address cannot reach the memory if the address did
/// not escape before the call, which it works out along the paths through the function.
/// Its loop passes, which take an address that escapes anywhere in the function to escape
/// everywhere, know then that a call in a loop of `&mut self`'s fields leaves the fields
/// alone when `self` is passed on only after the loop.
fn wrap<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    inner: FunctionValue<'ctx>,
    weights: Option<&super::tier::Weights>,
) -> Result<(), Unhandled> {
    let outer = module.add_function(OWN, inner.get_type(), Some(Linkage::External));
    let places = (0..inner.count_params()).map(AttributeLoc::Param);
    for loc in [AttributeLoc::Function, AttributeLoc::Return].into_iter().chain(places) {
        for attribute in inner.attributes(loc) {
            outer.add_attribute(loc, attribute);
        }
    }
    if let Some(weights) = weights {
        lower::entry_count(context, outer, weights.entered);
    }
    inner.set_linkage(Linkage::Internal);
    // The outer function keeps `#[inline(never)]`'s `noinline`; the inner one is only ever
    // built into it, and LLVM refuses a function marked both.
    inner.remove_enum_attribute(AttributeLoc::Function, attribute_kind("noinline"));
    let always = attribute_kind("alwaysinline");
    inner.add_attribute(AttributeLoc::Function, context.create_enum_attribute(always, 0));
    let builder = context.create_builder();
    builder.position_at_end(context.append_basic_block(outer, ""));
    let args = outer
        .get_param_iter()
        .map(Into::into)
        .collect::<SmallVec<[inkwell::values::BasicMetadataValueEnum<'ctx>; 8]>>();
    let call = builder.build_direct_call(inner, &args, "")?;
    // LLVM's inliner keeps a `musttail` call of the inner function one only where the call it
    // builds the function into is one too, so the outer call is, with the attributes of what it
    // passes on: else `become` would grow the stack by a frame each time.
    if lower::has_musttail(inner) {
        Lowering::same_abi_attributes(outer, call)
            // alloc: the error's message
            .map_err(|why| {
                Unhandled::TailCall(format!("the call of the wrapped function: {why}"))
            })?;
        call.set_tail_call_kind(inkwell::values::LLVMTailCallKind::LLVMTailCallKindMustTail);
    }
    match call.try_as_basic_value().basic() {
        Some(returned) => builder.build_return(Some(&returned))?,
        None => builder.build_return(None)?,
    };
    Ok(())
}

/// The CPU of this host and its features as LLVM names them (`+avx2,-avx512f,...`).
pub(crate) fn host_cpu() -> (String, String) {
    use inkwell::llvm_sys::{core::LLVMDisposeMessage, target_machine as tm};
    /// The text of `message`, which is then disposed of.
    ///
    /// # Safety
    ///
    /// `message` is a NUL-terminated string that LLVM allocated, that the caller owns and
    /// that nothing reads after this.
    unsafe fn owned(message: *mut std::ffi::c_char) -> String {
        // SAFETY: the caller's: `message` is NUL-terminated and live until disposed of below.
        let text = unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy().into_owned();
        // SAFETY: the caller's: LLVM allocated `message`, and nothing reads it after this.
        unsafe { LLVMDisposeMessage(message) };
        text
    }
    // SAFETY: it reads only the host's CPU and needs no LLVM state set up first.
    let name = unsafe { tm::LLVMGetHostCPUName() };
    // SAFETY: `name` is LLVM's string from just above, owned here and taken once.
    let name = unsafe { owned(name) };
    // SAFETY: LLVMGetHostCPUFeatures only reads the host CPU and needs no LLVM state set
    // up first; it returns a string LLVM allocated, which this code now owns.
    let features = unsafe { tm::LLVMGetHostCPUFeatures() };
    // SAFETY: `features` is the NUL-terminated string LLVMGetHostCPUFeatures just
    // allocated, owned here and given to `owned` once, which disposes of it; a second use
    // would read freed memory.
    let features = unsafe { owned(features) };
    (name, features)
}

/// The kind of LLVM's attribute `name`. LLVM answers 0 for a name it does not know (one it
/// renamed or removed, as LLVM 21 did `nocapture`), and an attribute of kind 0 crashes its
/// verifier, so an unknown name stops here.
pub(crate) fn attribute_kind(name: &str) -> u32 {
    let kind = Attribute::get_named_enum_kind_id(name);
    assert_ne!(kind, 0, "the linked LLVM has no attribute `{name}`");
    kind
}

/// `getelementptr i8, ptr base, by`: `base` moved by `by` bytes, which may be any number:
/// not `inbounds`, so no offset makes it poison.
fn byte_offset<'ctx>(
    builder: &Builder<'ctx>,
    context: &'ctx Context,
    base: PointerValue<'ctx>,
    by: IntValue<'ctx>,
) -> Result<PointerValue<'ctx>, BuilderError> {
    // SAFETY: inkwell's GEP is unsafe because indices that do not fit the type are invalid IR,
    // on which LLVM crashes. Here the type is `i8` and there is one index, a scalar integer
    // (an `IntValue` is never a vector), which LLVM takes for any `by`. `builder`, `base` and
    // `by` are of the one context `'ctx` names, as every inkwell call here takes them to be.
    unsafe { builder.build_gep(context.i8_type(), base, &[by], "") }
}

/// [`byte_offset`] marked `inbounds`, as rustc emits `<*const T>::offset`: the claim that
/// `base` and the result are in one allocation, which lets LLVM tell what the result is based
/// on. A wrong claim makes the compiled program's result poison; it cannot make this process
/// unsound, so the caller says why it holds.
fn in_bounds_byte_offset<'ctx>(
    builder: &Builder<'ctx>,
    context: &'ctx Context,
    base: PointerValue<'ctx>,
    by: IntValue<'ctx>,
) -> Result<PointerValue<'ctx>, BuilderError> {
    // SAFETY: as in `byte_offset`: an `i8` element and one scalar integer index make valid IR
    // for any `by`, of the one context `'ctx`. `inbounds` is a statement about the program
    // being compiled, not about this process.
    unsafe { builder.build_in_bounds_gep(context.i8_type(), base, &[by], "") }
}

/// The indexed profile `LLVMBlazeWriteProfile` writes of `records` (name, hash, counts), as
/// `llvm-profdata merge` does. What a test uses a profile of ([`pgo::written`]).
#[cfg(test)]
fn written_profile(records: &[(&str, u64, &[u64])]) -> Result<Vec<u8>, Unhandled> {
    use inkwell::llvm_sys::transforms::pass_builder::{
        LLVMBlazeProfileRecord, LLVMBlazeWriteProfile,
    };
    let records: Vec<LLVMBlazeProfileRecord> = records
        .iter()
        .map(|&(name, hash, counts)| LLVMBlazeProfileRecord {
            Name: name.as_ptr().cast(),
            NameLength: name.len(),
            Hash: hash,
            Counts: counts.as_ptr(),
            CountsLength: counts.len(),
        })
        .collect();
    let mut out = std::ptr::null_mut();
    // SAFETY: each record points at a name and counts borrowed from `records`' caller, which
    // outlive the call, with their lengths (no NUL needed); `out` is written once, with a
    // buffer this then owns. A record outliving its bytes would have LLVM read freed memory.
    let failed = unsafe { LLVMBlazeWriteProfile(records.as_ptr(), records.len(), &mut out) };
    // SAFETY: `failed` is what LLVMBlazeWriteProfile just returned, taken nowhere else.
    unsafe { Kept::taken(failed) }?;
    if out.is_null() {
        return Err(Unhandled::Llvm("LLVM wrote no profile".to_owned()));
    }
    // SAFETY: `out` is the non-null buffer LLVMBlazeWriteProfile just made, owned by nothing
    // else: the `MemoryBuffer` disposes of it once, when dropped below. Wrapped twice it would
    // be freed twice.
    let buffer = unsafe { MemoryBuffer::new(out) };
    Ok(buffer.as_slice().to_vec())
}

/// Tells LLVM what the function's `#[inline]` asks, as rustc does.
fn mark_hint<'ctx>(context: &'ctx Context, function: FunctionValue<'ctx>, hint: Hint) {
    let hint = match hint {
        Hint::None => return,
        Hint::Inline => "inlinehint",
        Hint::Always => "alwaysinline",
        Hint::Never => "noinline",
    };
    let hint = attribute_kind(hint);
    function.add_attribute(AttributeLoc::Function, context.create_enum_attribute(hint, 0));
}

/// Gives `function` the `target-features` of `features` ([`lower::Lowering::add`], which tells
/// it its `target-cpu` too), as rustc gives each function its own: the whole set, since LLVM
/// compiles a function that has one with it in place of its target machine's. The attribute
/// goes with the function's export, so LLVM's inliner builds it only into a caller that has
/// its features. Nothing for the target's baseline.
fn mark_features<'ctx>(
    context: &'ctx Context,
    function: FunctionValue<'ctx>,
    target: &super::Target,
    features: super::Features,
) -> Result<(), Unhandled> {
    let told = told(target, features)?;
    if !told.is_empty() {
        function.add_attribute(
            AttributeLoc::Function,
            context.create_string_attribute("target-features", &told),
        );
    }
    Ok(())
}

/// `features` in LLVM's names for `target` ([`super::Target::llvm_features`]).
fn told(target: &super::Target, features: super::Features) -> Result<String, Unhandled> {
    Ok(target.llvm_features(features))
}

/// The passes an export is simplified by before compiles import it: ThinLTO's pre-link
/// pipeline, which leaves loops to be unrolled and vectorized where the body is built in,
/// after the slots are taken apart as for a compile ([`Optimization::pipeline`]).
fn exported(optimization: Optimization) -> &'static str {
    match optimization.passes {
        Passes::Aggressive => {
            "function(sroa,instsimplify,sroa,jump-threading),thinlto-pre-link<O3>"
        }
        _ => "function(sroa,instsimplify,sroa,jump-threading),thinlto-pre-link<O2>",
    }
}

/// The export of a function for the compiles that call it ([`super::imports`]): its module
/// alone, with no line table nor variables, in bitcode once LLVM's pre-link passes are done
/// with it, and how many instructions it has then.
pub(super) fn export(
    recorder: &Wants,
    func: &ir::Function,
    pointees: &[(u32, super::fragment::Pointee)],
    hint: Hint,
    optimization: Optimization,
) -> Result<(std::sync::Arc<[u8]>, usize), Unhandled> {
    if func.dfg.num_insts() > super::LARGE {
        return Err(Unhandled::Shape("a body too large to export"));
    }
    Machine::with(recorder.target, optimization, |context, machine| {
        let module = context.create_module("blaze");
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());
        // alloc: once per export; `Wants` (fragment.rs) lists these, and the lowering looks them up
        let constants: HashMap<u32, &[u8], Global> = recorder.constants().collect();
        // alloc: once per export, as `constants`
        let statics: HashSet<u32, Global> = recorder.statics().collect();
        let own = Defined { name: OWN, func, pointees, named: &[], weights: None, marked: false };
        let function = Lowering::add(
            context,
            &module,
            recorder.target,
            &own,
            super::backend::V1_FRAME_POINTERS,
            crate::Features::BASELINE,        )?;
        mark_hint(context, function, hint);
        let shown = HashMap::new_in(Global);
        let (resume, memcmp) = (recorder.resume(), recorder.memcmp());
        let blocks_asm = recorder.asm_blocks();
        Lowering::define(
            context,
            &module,
            recorder.target,
            function,
            &own,
            &constants,
            &statics,
            resume,
            memcmp,
            &blocks_asm,
            &shown,
            None,
        )?;
        module.verify().map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        machine.run(&module, exported(optimization), false)?;
        let size = function.get_basic_blocks().into_iter().map(|it| it.get_instructions().count());
        Ok((module.write_bitcode_to_memory().as_slice().into(), size.sum()))
    })
}

/// Puts the body each of `imports` has in `module`, of `linkage` under the name of the request
/// it is of, with what it refers to under the names of the compile's requests.
///
/// LLVM's linker goes over all of the module it links into, so each linked into a unit's
/// module, which every one makes larger, took time of the square of their number: lowering
/// ripgrep's units took 23.7 s summed over the cores, where its functions alone took 12.3 s.
/// They are linked to each other two at a time instead, and into `module` at once.
fn link<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    imports: &[Import],
    linkage: Linkage,
) -> Result<(), Unhandled> {
    let llvm = |e: inkwell::support::LLVMString| Unhandled::Llvm(e.to_string_lossy().into_owned());
    let mut modules = Vec::with_capacity(imports.len());
    for import in imports {
        modules.push(imported(context, module, import)?);
    }
    while modules.len() > 1 {
        let mut linked = Vec::with_capacity(modules.len().div_ceil(2));
        let mut pairs = modules.into_iter();
        while let Some(into) = pairs.next() {
            if let Some(other) = pairs.next() {
                into.link_in_module(other).map_err(llvm)?;
            }
            linked.push(into);
        }
        modules = linked;
    }
    let Some(all) = modules.pop() else { return Ok(()) };
    module.link_in_module(all).map_err(llvm)?;
    for import in imports {
        let name = Symbol::Func(import.id).named();
        let body = module.get_function(&name).ok_or(Unhandled::Shape("an import not linked"))?;
        body.set_linkage(linkage);
    }
    Ok(())
}

/// The module of the body `import` has, under the name of the request it is of, with what it
/// refers to under the names of the compile's requests; and `module` with a declaration of
/// it, for LLVM to link it to.
fn imported<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    import: &Import,
) -> Result<Module<'ctx>, Unhandled> {
    let llvm = |e: inkwell::support::LLVMString| Unhandled::Llvm(e.to_string_lossy().into_owned());
    let buffer = MemoryBuffer::create_from_memory_range(&import.export.bitcode, "import");
    let imported = context.create_module_from_ir(buffer).map_err(llvm)?;
    let own = imported.get_function(OWN).ok_or(Unhandled::Shape("an export without its body"))?;
    let own_name = Symbol::Func(import.id).named();
    let renamed = |name: &std::ffi::CStr| match Symbol::parse(name.to_str().ok()?)? {
        Symbol::Func(at) => Some(Symbol::Func(*import.funcs.get(at as usize)?).named()),
        Symbol::Data(at) => Some(Symbol::Data(*import.datas.get(at as usize)?).named()),
    };
    let mut functions: SmallVec<[(FunctionValue<'ctx>, SymbolName); 16]> = SmallVec::new();
    let mut next = imported.get_first_function();
    while let Some(function) = next {
        next = function.get_next_function();
        if function == own {
            continue;
        }
        let Some(name) = renamed(function.get_name()) else { continue };
        // A call of itself is of the body.
        if name == own_name {
            function.replace_all_uses_with(own);
            // SAFETY: `function` is owned by `imported` and is not `own` (skipped above);
            // `replace_all_uses_with(own)` just above left no use of it, and neither `names`
            // nor the loop keeps it (`next` was read before), so freeing it leaves nothing
            // dangling.
            unsafe { function.delete() };
            continue;
        }
        functions.push((function, name));
    }
    let mut datas: SmallVec<[(GlobalValue<'ctx>, SymbolName); 16]> = SmallVec::new();
    for global in imported.get_globals() {
        if let Some(name) = renamed(global.get_name()) {
            datas.push((global, name));
        }
    }
    let function_name = |it: FunctionValue<'ctx>| it.get_name().to_string_lossy().into_owned();
    one_per_request(
        &mut functions,
        import,
        function_name,
        |it| it.count_basic_blocks() > 0,
        |_, _| false,
        |kept, gone| {
            gone.replace_all_uses_with(kept);
            // SAFETY: `gone` is a function of `imported` that `functions` no longer holds
            // (`one_per_request` drops it), and `replace_all_uses_with` just above left no use of
            // it, so freeing it leaves nothing dangling.
            unsafe { gone.delete() };
        },
    )?;
    let data_name = |it: GlobalValue<'ctx>| it.get_name().to_string_lossy().into_owned();
    let defined = |it: GlobalValue<'ctx>| it.get_initializer().is_some();
    let same =
        |a: GlobalValue<'ctx>, b: GlobalValue<'ctx>| a.get_initializer() == b.get_initializer();
    one_per_request(&mut datas, import, data_name, defined, same, |kept, gone| {
        gone.as_pointer_value().replace_all_uses_with(kept.as_pointer_value());
        // SAFETY: `gone` is a global variable of `imported` (from `get_globals`) that `datas`
        // no longer holds, and `replace_all_uses_with` just above left no use of it, so
        // freeing it leaves nothing dangling.
        unsafe { gone.delete() };
    })?;
    let mut names: SmallVec<[(GlobalValue<'ctx>, SymbolName); 16]> = SmallVec::new();
    names.push((own.as_global_value(), own_name));
    names.extend(functions.iter().map(|&(function, name)| (function.as_global_value(), name)));
    names.extend(datas);
    // In two steps, as a name given may be one another still has.
    for (at, (global, _)) in names.iter().enumerate() {
        global.set_name(&SymbolName::of("blaze.import.", at as u32));
    }
    for (global, name) in &names {
        global.set_name(name);
        // Each name is one request's and given once above, so none is taken: LLVM would make
        // it another (`blaze.d.3.1`), which no request is.
        if global.get_name().to_bytes() != name.bare().as_bytes() {
            // alloc: the error's message
            return Err(Unhandled::Duplicate(format!(
                "the import of `{}` names `{}` twice, and LLVM called the second `{}`",
                own_name.bare(),
                name.bare(),
                global.get_name().to_string_lossy()
            )));
        }
    }
    // Of its linkage only once in `module`: LLVM links in a body that is only available
    // externally only where the module it goes to has the function, and the imports are
    // linked to each other first.
    own.set_linkage(Linkage::External);
    if module.get_function(&own_name).is_none() {
        module.add_function(&own_name, own.get_type(), Some(Linkage::External));
    }
    Ok(imported)
}

/// Leaves one of `items`, the functions or the data objects of an import's module with the
/// names of the compile's requests they are to have, for each name. Two requests of the
/// export can be one of the compile: `v2::release`'s `merge` takes two equal ones (a file's
/// path or a place asked for twice) as one, and the request is then the one object. Named
/// both, LLVM would call the second `blaze.d.3.1`, which no request is, and the code would
/// refer to nothing (an undefined symbol at link, in 2 of 4 cold self-builds on 2026-11-02).
/// The uses of the others go to the one kept, which is the one with a definition, by `merge`
/// (kept, gone); the others are dropped from `items`. Two with different definitions are
/// refused, naming both by their names in the export (`name_of`, read before any is renamed).
fn one_per_request<T: Copy>(
    items: &mut SmallVec<[(T, SymbolName); 16]>,
    import: &Import,
    name_of: impl Fn(T) -> String,
    defined: impl Fn(T) -> bool,
    same: impl Fn(T, T) -> bool,
    merge: impl Fn(T, T),
) -> Result<(), Unhandled> {
    if items.len() < 2 {
        return Ok(());
    }
    // In the order of their names, then of the module, so the one kept is the same each time.
    // alloc: on the stack up to 16, else once per import with more, its own objects
    let mut order: SmallVec<[usize; 16]> = (0..items.len()).collect();
    order.sort_unstable_by(|&a, &b| items[a].1.bare().cmp(items[b].1.bare()).then(a.cmp(&b)));
    let mut gone: SmallVec<[usize; 4]> = SmallVec::new();
    for run in order.chunk_by(|&a, &b| items[a].1.bare() == items[b].1.bare()) {
        if run.len() < 2 {
            continue;
        }
        let kept = run.iter().copied().find(|&it| defined(items[it].0)).unwrap_or(run[0]);
        for &other in run {
            if other == kept {
                continue;
            }
            let (keep, it) = (items[kept].0, items[other].0);
            if defined(it) && !same(keep, it) {
                // alloc: the error's message
                return Err(Unhandled::Duplicate(format!(
                    "the import of `{}` has `{}` and `{}` of its export, defined differently, as \
                     the one request `{}`",
                    Symbol::Func(import.id),
                    name_of(keep),
                    name_of(it),
                    items[kept].1.bare(),
                )));
            }
            merge(keep, it);
            gone.push(other);
        }
    }
    gone.sort_unstable();
    for at in gone.into_iter().rev() {
        items.remove(at);
    }
    Ok(())
}

/// Has each call in the bodies linked in ([`link`]) made through the type its callee has in
/// `module`. An export declares what it calls with the types it calls it through, and a
/// parameter that one has as a pointer another has as an integer of its width
/// ([`Lowering::same_but_pointers`]), so a body linked in can call another with a type it
/// does not have. LLVM takes such a call of a body to be undefined: of `ptr::write`, imported
/// through a function that passed it the address as an integer, it kept nothing of what was
/// written. A call of a body through a type that differs in more than that is refused, as one
/// request is of one signature; one of a function only declared is left, as a C variadic
/// one is called with the arguments it is given.
fn agree_calls<'ctx>(context: &'ctx Context, module: &Module<'ctx>) -> Result<(), Unhandled> {
    use inkwell::{
        basic_block::BasicBlock,
        llvm_sys::{core, debuginfo},
        types::{BasicMetadataTypeEnum, FunctionType},
        values::{
            AsValueRef as _, BasicMetadataValueEnum, BasicValueEnum, CallSiteValue,
            InstructionOpcode, InstructionValue,
        },
    };
    let builder = context.create_builder();
    for function in module.get_functions() {
        for block in function.get_basic_block_iter() {
            let mut next = block.get_first_instruction();
            while let Some(instruction) = next {
                next = instruction.get_next_instruction();
                let opcode = instruction.get_opcode();
                if !matches!(opcode, InstructionOpcode::Call | InstructionOpcode::Invoke) {
                    continue;
                }
                // SAFETY: `instruction` is a call or an invoke (opcode checked just above)
                // owned by `module`, which outlives this walk; `CallSiteValue` on anything
                // else would read the wrong operand layout.
                let call = unsafe { CallSiteValue::new(instruction.as_value_ref()) };
                // SAFETY: `instruction` is a live call or invoke of `module`, so it has a
                // called operand, owned by `module` for this walk. Not `get_called_fn_value`,
                // which has no function for a call of another type.
                let callee = unsafe { core::LLVMGetCalledValue(instruction.as_value_ref()) };
                // SAFETY: `callee` is the called operand just read, a value of `module` live
                // for this walk; LLVMIsAFunction only reads its kind.
                if unsafe { core::LLVMIsAFunction(callee) }.is_null() {
                    continue;
                }
                // SAFETY: LLVMIsAFunction just above said `callee` is a function of `module`,
                // live for `'ctx`; a non-function would make inkwell call function-only C APIs
                // on it.
                let Some(callee) = (unsafe { FunctionValue::new(callee) }) else { continue };
                if callee.count_basic_blocks() == 0 {
                    continue;
                }
                // SAFETY: `instruction` is a live call or invoke of `module`; the type it is
                // called through is owned by `context` and lives for `'ctx`.
                let called = unsafe { core::LLVMGetCalledFunctionType(instruction.as_value_ref()) };
                // SAFETY: `called` is a function type of `context`, which outlives `'ctx`; a
                // non-function type would make `FunctionType` lie about its kind.
                let through = unsafe { FunctionType::new(called) };
                let ty = callee.get_type();
                if through == ty {
                    continue;
                }
                // A `musttail` call is of the caller's type, which its callee's must then be:
                // turned into a call of the callee's type it would not verify.
                if call.get_tail_call_kind()
                    == inkwell::values::LLVMTailCallKind::LLVMTailCallKindMustTail
                {
                    // alloc: the error's message
                    return Err(Unhandled::TailCall(format!(
                        "`{}` tail-calls `{}`, whose body has parameters that are pointers where \
                         the caller's are integers or the other way (`{}` where the caller is \
                         `{}`), which a `musttail` call can't convert",
                        function.get_name().to_string_lossy(),
                        callee.get_name().to_string_lossy(),
                        ty.print_to_string().to_string_lossy(),
                        through.print_to_string().to_string_lossy(),
                    )));
                }
                if !Lowering::same_but_pointers(through, ty) {
                    return Err(Unhandled::Shape("an import called with another signature"));
                }
                builder.position_before(&instruction);
                let params = ty.get_param_types();
                let mut args: SmallVec<[BasicValueEnum<'ctx>; 8]> =
                    SmallVec::with_capacity(params.len());
                for (at, param) in params.iter().enumerate() {
                    let arg = instruction
                        .get_operand(at as u32)
                        .and_then(|it| it.value())
                        .ok_or(Unhandled::Shape("a call without its arguments"))?;
                    args.push(match (*param, arg) {
                        (BasicMetadataTypeEnum::PointerType(to), BasicValueEnum::IntValue(int)) => {
                            builder.build_int_to_ptr(int, to, "")?.into()
                        }
                        (BasicMetadataTypeEnum::IntType(to), BasicValueEnum::PointerValue(ptr)) => {
                            builder.build_ptr_to_int(ptr, to, "")?.into()
                        }
                        _ => arg,
                    });
                }
                let made = match opcode {
                    InstructionOpcode::Invoke => {
                        let at = instruction.as_value_ref();
                        // SAFETY: `at` is an invoke (this arm of the opcode match) of `module`,
                        // live for this walk, and an invoke always has a normal destination.
                        let then = unsafe { core::LLVMGetNormalDest(at) };
                        // SAFETY: `then` is a block of `function`, owned by `module` for
                        // `'ctx`; `BasicBlock::new` maps null to `None`.
                        let then = unsafe { BasicBlock::new(then) };
                        // SAFETY: `at` is a live invoke of `module`, which always has an
                        // unwind destination.
                        let catch = unsafe { core::LLVMGetUnwindDest(at) };
                        // SAFETY: `catch` is a block of `function`, owned by `module` for
                        // `'ctx`; null maps to `None`.
                        let catch = unsafe { BasicBlock::new(catch) };
                        let (Some(then), Some(catch)) = (then, catch) else {
                            return Err(Unhandled::Shape("an invoke without its destinations"));
                        };
                        builder.build_direct_invoke(callee, &args, then, catch, "")?
                    }
                    _ => {
                        let args = args
                            .iter()
                            .map(|&it| it.into())
                            .collect::<SmallVec<[BasicMetadataValueEnum<'ctx>; 8]>>();
                        let made = builder.build_direct_call(callee, &args, "")?;
                        made.set_tail_call_kind(call.get_tail_call_kind());
                        made
                    }
                };
                made.set_call_convention(call.get_call_convention());
                // What a parameter that is a pointer here and an integer there is told is of
                // the one it was.
                let through_params = through.get_param_types();
                let places = (0..call.count_arguments())
                    .filter(|&at| through_params.get(at as usize) == params.get(at as usize))
                    .map(AttributeLoc::Param);
                for loc in [AttributeLoc::Function, AttributeLoc::Return].into_iter().chain(places)
                {
                    for attribute in call.attributes(loc) {
                        made.add_attribute(loc, attribute);
                    }
                }
                // SAFETY: `made` is the call or invoke just built into `module`, an
                // instruction owned by it for `'ctx`.
                let made = unsafe { InstructionValue::new(made.as_value_ref()) };
                // SAFETY: `instruction` is a live instruction of `module`; its location is
                // metadata owned by `context`, which outlives the module.
                let location =
                    unsafe { debuginfo::LLVMInstructionGetDebugLoc(instruction.as_value_ref()) };
                if !location.is_null() {
                    // SAFETY: `made` is the instruction just built in `module`, and `location`
                    // non-null metadata of the same context, which outlives both.
                    unsafe { debuginfo::LLVMInstructionSetDebugLoc(made.as_value_ref(), location) };
                }
                instruction.replace_all_uses_with(&made);
                instruction.erase_from_basic_block();
            }
        }
    }
    Ok(())
}

/// What the body of the requested function `id` that LLVM is shown is called in the module.
/// No symbol of a program has such a name.
fn shown_name(id: u32) -> SymbolName {
    SymbolName::of("blaze.a.", id)
}

/// Has every call of a body shown that LLVM kept call the function itself, and drops the
/// bodies: only the function being compiled is in the object. What LLVM made of the calls
/// from what it read in the bodies holds of the functions, which are what the bodies are of
/// (an edit of one sends the function being compiled back to level 1).
fn unshow<'ctx>(
    module: &Module<'ctx>,
    shown: impl IntoIterator<Item = (u32, FunctionValue<'ctx>)>,
) {
    for (id, body) in shown {
        let name = Symbol::Func(id).named();
        let function = module.get_function(&name).unwrap_or_else(|| {
            module.add_function(&name, body.get_type(), Some(Linkage::External))
        });
        let address = body.as_global_value().as_pointer_value();
        address.replace_all_uses_with(function.as_global_value().as_pointer_value());
        // SAFETY: `body` is owned by `module`, and `replace_all_uses_with` just above sent every
        // use of its address to `function`; this loop reads each body once and the caller does
        // not read `shown` after `unshow`, so freeing it leaves nothing dangling.
        unsafe { body.delete() };
    }
}

/// The object file LLVM's assembler makes of `text`, which is assembly in the syntax
/// `asm!` is written in. Assembly LLVM rejects is an error naming what it said
/// ([`diagnosed`]).
pub(super) fn assemble(target: &'static super::Target, text: &str) -> Result<Vec<u8>, String> {
    Machine::with(target, Optimization::NONE, |context, machine| {
        let module = context.create_module("blaze_asm");
        module.set_triple(&machine.get_triple());
        // What rustc wraps `global_asm!` in, where there are two syntaxes.
        let text = match target.arch {
            // alloc: once per block of assembly the store does not have (`asm::assemble`)
            Arch::X86_64 => format!(".intel_syntax noprefix\n{text}\n.att_syntax\n"),
            // alloc: as above
            Arch::Aarch64 | Arch::Wasm32 | Arch::Wasm64 => format!("{text}\n"),
        };
        module.set_inline_assembly(&text);
        let object = machine
            .write_to_memory_buffer(&module, FileType::Object)
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        // alloc: the object, which the caller keeps
        Ok(object.as_slice().to_vec())
    })
    // alloc: the error's message
    .map_err(|e| format!("inline assembly: {e}"))
}

/// Sets the options of LLVM that only its command line reaches, for the process.
///
/// On AArch64 the quick selection is not the global instruction selector. That one deletes
/// the unused copy of the selector register at a landing pad and leaves the register a
/// live-in of the block, and the fast register allocator (LLVM 20.1, where this was found)
/// does not reload a value it has put in a live-in register: after optimization, which keeps
/// values in registers across an unwind edge, a landing pad then reads the selector where it
/// wants a drop flag (`breakers/cases/src/unwind_many_flags.rs`).
/// LLVM's inliner threshold: 500, over the level's own (225 at -O2, 250 at -O3). Measured on
/// LLVM 23 against 225 and 1000, and against leaving every call to LLVM's inliner (docs/tiers.md,
/// "LLVM's inliner"): 500 takes the runtime set's geomean from 1.091 to 1.080 of rustc's for
/// 2 to 5% more code and no more compile time; 1000 is no faster (1.079) for twice the code.
const INLINE_THRESHOLD: u32 = 500;

/// [`INLINE_THRESHOLD`], or the threshold `BLAZE_LLVM_INLINE_THRESHOLD=N` names. No key has it:
/// code inlined at another threshold is other code, so a unit refuses to be built with it set,
/// like the other variables of `v2::backend`'s `CHANGES_CODE`. To measure another threshold,
/// change [`INLINE_THRESHOLD`] and rebuild.
pub(super) fn inline_threshold() -> u32 {
    std::env::var("BLAZE_LLVM_INLINE_THRESHOLD").map_or(INLINE_THRESHOLD, |it| {
        it.parse().unwrap_or_else(|_| panic!("BLAZE_LLVM_INLINE_THRESHOLD={it} is not a number"))
    })
}

fn set_options(arch: Arch) {
    static SET: Once = Once::new();
    SET.call_once(|| {
        // alloc: once per process
        let mut arguments = vec![c"blaze".as_ptr()];
        if arch == Arch::Aarch64 {
            arguments.push(c"-aarch64-enable-global-isel-at-O=-1".as_ptr());
        }
        // alloc: once per process
        let threshold = std::ffi::CString::new(format!("-inline-threshold={}", inline_threshold()))
            .expect("no NUL");
        arguments.push(threshold.as_ptr());
        // Both are read by PGO's passes only, which run for a unit built with PGO alone
        // (`pgo`): rustc's inliner before instrumentation is off, as rustc turns it off for
        // `-Cprofile-generate` and `-Cprofile-use`; and value profiling is off (`pgo`'s notes).
        arguments.push(c"-disable-preinline".as_ptr());
        arguments.push(c"-disable-vp".as_ptr());
        // SAFETY: the arguments are NUL-terminated C strings (`c"..."` and `threshold`, a
        // `CString`) that outlive the call, and LLVM is given their number. LLVM's option
        // registry is process-wide and unsynchronized: `SET` makes this its one write, and
        // every thread reaches it before its first target machine (`Machine::create_for`),
        // waiting in `call_once` until it is done. A second, racing parse would be a data race
        // in LLVM.
        unsafe {
            LLVMParseCommandLineOptions(
                arguments.len() as i32,
                arguments.as_ptr(),
                std::ptr::null(),
            )
        }
    });
}

impl Optimization {
    /// The passes to run. What `lower` writes keeps every value in a stack slot, and a slot
    /// indexed by a number that is itself in a slot is not taken apart until that number is
    /// known; left to the default pipeline, such a slot is deleted whole and the variable it
    /// holds is lost to the debugger. So the slots are taken apart twice first, with the
    /// constants folded in between.
    ///
    /// Then branches are threaded. A call Cranelift built in returns through a block that
    /// joins its returns, and the caller branches on what it returned there: an `Option`'s
    /// discriminant. Left to the default pipeline, the join is folded into `select`s first,
    /// and a loop over a range counts with `i + (i < n)`, which LLVM can neither unroll nor
    /// prove an index of in bounds or free of overflow with. Threaded, it counts with `i + 1`.
    ///
    /// At `-O3` the bodies LLVM is shown are then built in, and the same is done again to what
    /// they joined, with values numbered once after it. A body LLVM builds in itself returns
    /// through a join as well, and the default pipeline numbers values before it threads
    /// branches: a range's start the loop over it advances to, `i + 1`, stays two values when
    /// the body of the loop computes it too, until instruction combining makes the inner loop
    /// of `for j in i + 1..n` count with `phi + 1`. An index scaled from that is not known
    /// free of overflow, so alias analysis cannot tell `bodies[i].velocity` from
    /// `bodies[j].mass`: every iteration reloads what it stored (nbody, 1.97 times
    /// release on Graviton4, 1.28 with the bodies built in first).
    ///
    /// At `-O3` what LLVM made redundant is then removed once more. A loop of a callee
    /// Cranelift built in is unrolled late in the pipeline, after LLVM's last numbering of
    /// values, while the copies are still joined by branches on constants: each copy keeps
    /// its own `%` and overflow check (16 `urem`s where rustc, which optimizes the callee on
    /// its own before building it in, has 7, in the Life benchmark's `step`). Instruction
    /// combining named in a pipeline checks that one run of it left nothing to combine and
    /// aborts the process when it did, which the default pipelines do not ask for.
    ///
    /// The levels above 1 run LLVM's pipelines as rustc's release profile does, whose codegen
    /// units go through ThinLTO within the crate: the one before linking, then the one at
    /// it, so every function is simplified twice. The second time a loop is small enough to
    /// unroll whole that the first left too big (nbody's loops over pairs of bodies).
    fn pipeline(self) -> Option<&'static str> {
        match self.passes {
            Passes::None => None,
            Passes::Less => Some("function(sroa,instsimplify,sroa),default<O1>"),
            Passes::Default => Some(
                "function(sroa,instsimplify,sroa,jump-threading),\
                 thinlto-pre-link<O2>,thinlto<O2>",
            ),
            Passes::Aggressive => Some(
                "function(sroa,instsimplify,sroa,jump-threading),\
                 cgscc(inline,function(sroa,instsimplify,sroa,jump-threading,early-cse<memssa>)),\
                 thinlto-pre-link<O3>,thinlto<O3>,\
                 function(gvn,sccp,instcombine<no-verify-fixpoint>,simplifycfg)",
            ),
        }
    }

    /// The level of the code generator, which is not that of the passes.
    fn level(self) -> OptimizationLevel {
        match (self.selection, self.passes) {
            (Selection::Fast, _) | (_, Passes::None) => OptimizationLevel::None,
            (Selection::Optimized, Passes::Less) => OptimizationLevel::Less,
            (Selection::Optimized, Passes::Default) => OptimizationLevel::Default,
            (Selection::Optimized, Passes::Aggressive) => OptimizationLevel::Aggressive,
        }
    }
}

thread_local! {
    /// The errors LLVM diagnosed in the context of this thread's [`Machine`] since it was
    /// last asked to work.
    // alloc: empty, allocates nothing until LLVM diagnoses an error
    static DIAGNOSED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// A context whose diagnostics come to [`DIAGNOSED`]. Through the C API, LLVM 23's own
/// handler prints an error and goes on: `mov ah, sil` in an `asm!` block is printed, left
/// out of the object, and `LLVMTargetMachineEmitToMemoryBuffer` succeeds with the rest.
fn diagnosed() -> Context {
    extern "C" fn handle(
        info: inkwell::llvm_sys::prelude::LLVMDiagnosticInfoRef,
        _: *mut core::ffi::c_void,
    ) {
        use inkwell::llvm_sys::{LLVMDiagnosticSeverity, core as llvm};
        // SAFETY: LLVM passes `info`, a diagnostic it owns that lives through this call; the
        // description is a new message this handler owns.
        let message = unsafe { llvm::LLVMGetDiagInfoDescription(info) };
        // SAFETY: `message` is the NUL-terminated string LLVM just allocated, live until
        // disposed of below.
        let text = unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy().into_owned();
        // SAFETY: `message` was allocated by LLVM for this handler and is not read after this.
        unsafe { llvm::LLVMDisposeMessage(message) };
        // SAFETY: `info` lives through this call.
        let severity = unsafe { llvm::LLVMGetDiagInfoSeverity(info) };
        match severity {
            LLVMDiagnosticSeverity::LLVMDSError => DIAGNOSED.with_borrow_mut(|it| it.push(text)),
            LLVMDiagnosticSeverity::LLVMDSWarning => eprintln!("LLVM warning: {text}"),
            LLVMDiagnosticSeverity::LLVMDSRemark | LLVMDiagnosticSeverity::LLVMDSNote => {}
        }
    }
    let context = Context::create();
    // SAFETY: `context` was just created and is owned here; `handle` is a `'static` function
    // that takes no context pointer, so the null one it is given is never read.
    unsafe {
        inkwell::llvm_sys::core::LLVMContextSetDiagnosticHandler(
            context.raw(),
            Some(handle),
            core::ptr::null_mut(),
        );
    }
    context
}

/// LLVM's pass builder for one target machine, with its analysis managers, instrumentation
/// and parsed pipelines (`vendor/llvm-sys/wrappers/passes.cpp`): what `LLVMRunPasses` builds
/// and tears down on every call, a process-wide lock among it, kept so that running a
/// module's passes costs what the passes do. Bound to the first context it runs a module of.
struct Kept(std::ptr::NonNull<LLVMBlazeOpaquePasses>);

impl Kept {
    fn of(machine: &TargetMachine) -> Kept {
        // SAFETY: `machine` is a live target machine; `Machinery` declares `passes` before
        // `machine`, so this object is dropped before the machine it points to. Dropped after,
        // its pipelines would use a freed machine.
        let made = unsafe { LLVMBlazeCreatePasses(machine.as_mut_ptr()) };
        Kept(std::ptr::NonNull::new(made).expect("LLVM made no passes"))
    }

    /// Passes whose pipelines instrument for PGO or use `profile` (the bytes of the profile
    /// `wanted` names, with a NUL after them, as [`pgo::profile`] keeps them).
    fn of_pgo(
        machine: &TargetMachine,
        wanted: pgo::Wanted,
        profile: Option<&[u8]>,
    ) -> Result<Kept, Unhandled> {
        use inkwell::llvm_sys::transforms::pass_builder::{
            LLVMBlazeCreatePgoPasses, LLVMBlazePgo, LLVMBlazePgoGenerate, LLVMBlazePgoUse,
        };
        let (action, bytes) = match (wanted, profile) {
            (pgo::Wanted::Generate, _) => (LLVMBlazePgoGenerate, &[][..]),
            (pgo::Wanted::Use(_), Some(bytes @ [.., 0])) => (LLVMBlazePgoUse, bytes),
            (pgo::Wanted::Use(id), _) => {
                // alloc: the error's message
                return Err(Unhandled::Llvm(format!(
                    "the profile {id:x} without its bytes and their NUL"
                )));
            }
        };
        let pgo = LLVMBlazePgo {
            Action: action,
            Profile: bytes.as_ptr().cast(),
            Length: bytes.len().saturating_sub(1),
        };
        // SAFETY: `machine` is live and outlives the passes, as in `Kept::of`; `pgo` is read
        // during the call only, and the profile's bytes it points at, followed by their NUL
        // (matched above), are kept by the `Machinery` after its passes, so they outlive them.
        let made = unsafe { LLVMBlazeCreatePgoPasses(machine.as_mut_ptr(), &pgo) };
        std::ptr::NonNull::new(made)
            .map(Kept)
            // alloc: the error's message
            .ok_or_else(|| Unhandled::Llvm("LLVM made no passes for PGO".to_owned()))
    }

    /// Runs PGO's passes alone over `module`, as LLVM does at `-O0`.
    fn run_pgo(&self, module: &Module<'_>) -> Result<(), Unhandled> {
        use inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeRunPgo;
        // SAFETY: as in `run`: `self.0` is live and this thread's, `module` is live for the
        // call and of the context the wrapper checks.
        let failed = unsafe { LLVMBlazeRunPgo(self.0.as_ptr(), module.as_mut_ptr()) };
        // SAFETY: `failed` is what LLVMBlazeRunPgo just returned, taken nowhere else.
        unsafe { Kept::taken(failed) }
    }

    /// How many functions the passes found a record of under their name with another hash in
    /// the profile they use, since this was last asked.
    fn take_mismatched(&self) -> u32 {
        use inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeTakeMismatched;
        // SAFETY: `self.0` is live and used by this thread only, as in `run`.
        unsafe { LLVMBlazeTakeMismatched(self.0.as_ptr()) }
    }

    /// How many functions the profile the passes use gave counts to (an entry count they had
    /// not), since this was last asked.
    fn take_applied(&self) -> u32 {
        use inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeTakeApplied;
        // SAFETY: `self.0` is live and used by this thread only, as in `run`.
        unsafe { LLVMBlazeTakeApplied(self.0.as_ptr()) }
    }

    /// Runs `passes` over `module`, vectorizing straight-line code if `slp`, as
    /// `Module::run_passes` would with `PassBuilderOptions::set_loop_slp_vectorization(slp)`
    /// and nothing else set.
    fn run(&self, module: &Module<'_>, passes: &str, slp: bool) -> Result<(), Unhandled> {
        // SAFETY: `self.0` is live until `Drop`, and `Kept` is `!Send` (a `NonNull`), so only
        // the thread that made it uses it; `module` is live for the call and the wrapper
        // refuses one of another context (`vendor/llvm-sys/wrappers/passes.cpp`); `passes` is
        // read for `passes.len()` bytes during the call only.
        let failed = unsafe {
            LLVMBlazeRunPasses(
                self.0.as_ptr(),
                module.as_mut_ptr(),
                passes.as_ptr().cast(),
                passes.len(),
                i32::from(slp),
            )
        };
        // SAFETY: `failed` is what LLVMBlazeRunPasses just returned, taken nowhere else.
        unsafe { Kept::taken(failed) }
    }

    /// Starts a report ([`report`]): the runs from now on catch the loop vectorizer's remarks.
    fn report_start(&self) {
        use inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeReportStart;
        // SAFETY: `self.0` is live and used by this thread only, as in `run`.
        unsafe { LLVMBlazeReportStart(self.0.as_ptr()) }
    }

    /// Ends a report: the runs from now on catch nothing, and what was caught is let go.
    fn report_stop(&self) {
        use inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeReportStop;
        // SAFETY: `self.0` is live and used by this thread only, as in `run`.
        unsafe { LLVMBlazeReportStop(self.0.as_ptr()) }
    }

    /// The report since [`Kept::report_start`]: the remarks caught, then what `function`'s
    /// optimized code keeps in its loops, each with its loop's heat, in the order LLVM made
    /// them ([`report::Raw`]).
    fn report_of(&self, function: FunctionValue<'_>) -> Result<Vec<report::Raw>, Unhandled> {
        use inkwell::llvm_sys::transforms::pass_builder::{
            LLVMBlazeReportKept, LLVMBlazeReportRow, LLVMBlazeReportRows,
        };
        use inkwell::values::AsValueRef as _;
        // SAFETY: `self.0` is live and this thread's (as in `run`); `function` is a live function
        // of a module of the context the passes ran, which the wrapper only reads.
        unsafe { LLVMBlazeReportKept(self.0.as_ptr(), function.as_value_ref()) };
        let mut rows: *const LLVMBlazeReportRow = core::ptr::null();
        // SAFETY: `self.0` is live; `rows` is written with a pointer to the wrapper's rows.
        let count = unsafe { LLVMBlazeReportRows(self.0.as_ptr(), &mut rows) };
        if count == 0 || rows.is_null() {
            // alloc: empty, allocates nothing
            return Ok(Vec::new());
        }
        // SAFETY: the wrapper's `count` rows start at `rows` and stay live until the next
        // `LLVMBlazeReport*` call on these passes, which none of what follows makes.
        let rows = unsafe { core::slice::from_raw_parts(rows, count) };
        let text = |at: *const core::ffi::c_char, len: usize| -> String {
            if at.is_null() || len == 0 {
                // alloc: empty, allocates nothing
                return String::new();
            }
            // SAFETY: `at` points at `len` bytes of a string the wrapper keeps live as `rows`.
            let bytes = unsafe { core::slice::from_raw_parts(at.cast::<u8>(), len) };
            // alloc: a finding's text, kept in the function's report
            String::from_utf8_lossy(bytes).into_owned()
        };
        // alloc: the function's findings, kept in its report
        rows.iter()
            .map(|row| {
                let (name, message) =
                    (text(row.Name, row.NameLength), text(row.Message, row.MessageLength));
                report::Raw::of(
                    row.Kind,
                    row.Heat,
                    row.Line,
                    row.LoopLine,
                    row.Count,
                    name,
                    message,
                )
            })
            .collect()
    }

    /// Instruments `module` for `wanted`: SanitizerCoverage, then MemorySanitizer,
    /// ThreadSanitizer and AddressSanitizer, as clang orders them.
    fn instrument(
        &self,
        module: &Module<'_>,
        wanted: &inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeSanitizers,
    ) -> Result<(), Unhandled> {
        use inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeRunSanitizers;
        // SAFETY: `self.0` is live and used by this thread only (as in `run`); `module` is live
        // for the call and the wrapper refuses one of another context; `wanted` is a live
        // `#[repr(C)]` struct of the wrapper's layout, read during the call only.
        let failed =
            unsafe { LLVMBlazeRunSanitizers(self.0.as_ptr(), module.as_mut_ptr(), wanted) };
        // SAFETY: `failed` is what LLVMBlazeRunSanitizers just returned, taken nowhere else.
        unsafe { Kept::taken(failed) }
    }

    /// The error a `LLVMBlaze*` call returned, null for none, as an `Unhandled`.
    ///
    /// # Safety
    ///
    /// `failed` is null or an error LLVM returned that nothing else owns or takes.
    unsafe fn taken(failed: inkwell::llvm_sys::error::LLVMErrorRef) -> Result<(), Unhandled> {
        use inkwell::llvm_sys::error::{LLVMDisposeErrorMessage, LLVMGetErrorMessage};
        if failed.is_null() {
            return Ok(());
        }
        // SAFETY: `failed` is a non-null error owned here and taken once (the caller's
        // contract); LLVMGetErrorMessage consumes it and returns a message this code owns.
        let raw = unsafe { LLVMGetErrorMessage(failed) };
        // SAFETY: `raw` is NUL-terminated and live until disposed of below.
        let message = unsafe { std::ffi::CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        // SAFETY: `raw` came from LLVMGetErrorMessage, is disposed of once and not read after.
        unsafe { LLVMDisposeErrorMessage(raw) };
        Err(Unhandled::Llvm(message))
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        // SAFETY: `self.0` was made by `LLVMBlazeCreatePasses` and `Kept` is not `Clone`, so
        // this is its one disposal; the machine it was made for is still live (field order of
        // `Machinery`).
        unsafe { LLVMBlazeDisposePasses(self.0.as_ptr()) }
    }
}

/// A target machine and the passes kept for it ([`Kept`]).
pub(super) struct Machinery {
    // Before the profile and the machine, which have to outlive them.
    passes: Kept,
    /// The profile the passes use (with a NUL after it), which they read from memory.
    _profile: Option<std::sync::Arc<[u8]>>,
    machine: TargetMachine,
}

impl Machinery {
    fn new(machine: TargetMachine) -> Machinery {
        Machinery { passes: Kept::of(&machine), _profile: None, machine }
    }

    /// Passes for `machine` that instrument for PGO or use the profile `wanted` names, which
    /// [`pgo::give`] was given.
    fn profiled(machine: TargetMachine, wanted: pgo::Wanted) -> Result<Machinery, Unhandled> {
        let profile = match wanted {
            pgo::Wanted::Generate => None,
            pgo::Wanted::Use(id) => Some(pgo::profile(id)?),
        };
        let passes = Kept::of_pgo(&machine, wanted, profile.as_deref())?;
        Ok(Machinery { passes, _profile: profile, machine })
    }

    /// Runs the pipeline `passes` over `module`, vectorizing straight-line code if `slp`.
    fn run(&self, module: &Module<'_>, passes: &str, slp: bool) -> Result<(), Unhandled> {
        self.passes.run(module, passes, slp)
    }

    /// Runs PGO's passes alone over `module`: for a [`Machinery::profiled`] one, at a level
    /// whose pipeline has none ([`pgo::in_pipeline`]).
    fn run_pgo(&self, module: &Module<'_>) -> Result<(), Unhandled> {
        self.passes.run_pgo(module)
    }

    /// How many functions the passes found a record of under their name with another hash in
    /// the profile they use, since this was last asked.
    fn take_mismatched(&self) -> u32 {
        self.passes.take_mismatched()
    }

    /// How many functions the profile the passes use gave counts to, since this was last
    /// asked.
    fn take_applied(&self) -> u32 {
        self.passes.take_applied()
    }

    /// Instruments `module` for the sanitizers `wanted` ([`sanitize::instrument`]).
    fn instrument(
        &self,
        module: &Module<'_>,
        wanted: &inkwell::llvm_sys::transforms::pass_builder::LLVMBlazeSanitizers,
    ) -> Result<(), Unhandled> {
        self.passes.instrument(module, wanted)
    }

    /// Runs `work` with the loop vectorizer's remarks caught, then gives what it returned with
    /// the report of `function` of what it made ([`report`]): its remarks and what its loops
    /// keep. The passes catch nothing after, whatever `work` returned.
    fn reported<'ctx, T>(
        &self,
        work: impl FnOnce() -> Result<(T, FunctionValue<'ctx>), Unhandled>,
    ) -> Result<(T, Vec<report::Raw>), Unhandled> {
        self.passes.report_start();
        let done =
            work().and_then(|(value, function)| Ok((value, self.passes.report_of(function)?)));
        self.passes.report_stop();
        done
    }
}

impl std::ops::Deref for Machinery {
    type Target = TargetMachine;

    fn deref(&self) -> &TargetMachine {
        &self.machine
    }
}

/// The target as LLVM knows it, one per thread and level: making one costs more than a
/// small function does. The context the modules are made in is kept with them, as a
/// machine holds on to things of the last context it compiled a module of: the instruction
/// selector of AArch64 keeps the source location of the last instruction it saw, and lets
/// go of it at the next function, by when a context of that one module would be gone.
#[derive(Default)]
struct Machine {
    // Before the context, which has to outlive them and their passes. By target (its
    // `Target::index`), level, what they build with for PGO and the target features they are
    // told: a daemon builds a dev job
    // with the host's and a release job with the baseline's.
    machines:
        HashMap<(usize, Optimization, Option<pgo::Wanted>, super::Features), Machinery, Global>,
    context: Option<Context>,
    /// How many modules were made in the context.
    modules: usize,
}

impl Machine {
    /// How many modules a context is kept for. The constants and source locations of all
    /// of them stay in it.
    const MODULES: usize = 1024;

    /// The thread's machine of `target` and `optimization` for the baseline: v1's compiles,
    /// the assembler.
    fn with<T>(
        target: &'static super::Target,
        optimization: Optimization,
        work: impl FnOnce(&Context, &Machinery) -> Result<T, Unhandled>,
    ) -> Result<T, Unhandled> {
        Self::with_pgo(target, optimization, None, super::Features::BASELINE, work)
    }

    /// [`Machine::with`], with passes that build for `pgo` ([`Machinery::profiled`]), told the
    /// target features `features` ([`Machine::create_for`]).
    fn with_pgo<T>(
        target: &'static super::Target,
        optimization: Optimization,
        pgo: Option<pgo::Wanted>,
        features: super::Features,
        work: impl FnOnce(&Context, &Machinery) -> Result<T, Unhandled>,
    ) -> Result<T, Unhandled> {
        thread_local! {
            static MACHINE: RefCell<Machine> = RefCell::default();
        }
        MACHINE.with_borrow_mut(|this| {
            if this.modules == Self::MODULES {
                *this = Machine::default();
            }
            this.modules += 1;
            let machine = match this.machines.entry((target.index(), optimization, pgo, features)) {
                Entry::Occupied(known) => known.into_mut(),
                Entry::Vacant(slot) => {
                    let machine = Self::create_for(target, optimization, features)?;
                    slot.insert(match pgo {
                        None => Machinery::new(machine),
                        Some(wanted) => Machinery::profiled(machine, wanted)?,
                    })
                }
            };
            let context = this.context.get_or_insert_with(diagnosed);
            DIAGNOSED.with_borrow_mut(Vec::clear);
            let done = work(context, machine);
            // An error LLVM diagnosed leaves its output wrong, not refused: the assembler
            // drops an instruction it cannot encode and the object is written all the same.
            let errors = DIAGNOSED.with_borrow_mut(core::mem::take);
            match done {
                Ok(_) if !errors.is_empty() => Err(Unhandled::Llvm(errors.join("; "))),
                done => done,
            }
        })
    }

    fn create(
        target: &super::Target,
        optimization: Optimization,
    ) -> Result<TargetMachine, Unhandled> {
        Self::create_for(target, optimization, super::Features::BASELINE)
    }

    /// A target machine of the CPU of `features` ([`super::Target::cpu_of`]) told its features
    /// in LLVM's names ([`super::Target::llvm_features`]), as rustc tells its own the features
    /// of `-C target-feature` and `-C target-cpu`. A CPU or feature LLVM's target lacks is an
    /// error naming it, never left out.
    fn create_for(
        emitted: &super::Target,
        optimization: Optimization,
        features: super::Features,
    ) -> Result<TargetMachine, Unhandled> {
        set_options(emitted.arch);
        match emitted.arch {
            Arch::X86_64 => Target::initialize_x86(&InitializationConfig::default()),
            Arch::Aarch64 => Target::initialize_aarch64(&InitializationConfig::default()),
            #[cfg(feature = "llvm-wasm")]
            Arch::Wasm32 | Arch::Wasm64 => {
                Target::initialize_webassembly(&InitializationConfig::default())
            }
            // The pinned LLVM (`llvm.json`) is built without the WebAssembly target, so its
            // initializer is not in the library this links.
            #[cfg(not(feature = "llvm-wasm"))]
            Arch::Wasm32 | Arch::Wasm64 => {
                return Err(Unhandled::Llvm(
                    "no LLVM producer for wasm32: blaze's LLVM (llvm.json) is built without the \
                     WebAssembly target, and blaze is built without `llvm-wasm`"
                        // alloc: the error's message
                        .to_owned(),
                ));
            }
        }
        let name = emitted.triple();
        let triple = TargetTriple::create(name);
        let target = Target::from_triple(&triple)
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into()))?;
        // The set's CPU (`-C target-cpu`, or the target's baseline), as rustc compiles for;
        // position-independent, as the rest of the program is. A wasm module is static, as
        // rustc's wasm targets are: its code is reached by index and its data placed at link.
        let told = told(emitted, features)?;
        let cpu = emitted.cpu_of(features);
        let relocation = match emitted.arch {
            Arch::X86_64 | Arch::Aarch64 => RelocMode::PIC,
            Arch::Wasm32 | Arch::Wasm64 => RelocMode::Static,
        };
        let machine = target
            .create_target_machine(
                &triple,
                cpu,
                &told,
                optimization.level(),
                relocation,
                CodeModel::Default,
            )
            // alloc: an error
            .ok_or_else(|| {
                Unhandled::Llvm(format!(
                    "no target machine for {name}'s CPU {cpu} with the features {told:?}"
                ))
            })?;
        // SAFETY: `machine` is a live target machine, and `cpu` is read for its length during
        // the call only.
        if unsafe { LLVMBlazeHasCpu(machine.as_mut_ptr(), cpu.as_ptr().cast(), cpu.len()) } == 0 {
            // alloc: an error
            return Err(Unhandled::Llvm(format!(
                "LLVM's {name} has no CPU {cpu:?} (-C target-cpu)"
            )));
        }
        for feature in told.split(',').filter(|it| !it.is_empty()) {
            let bare = &feature[1..];
            // SAFETY: `machine` is a live target machine, and `bare` is read for its length
            // during the call only.
            let known = unsafe {
                LLVMBlazeHasFeature(machine.as_mut_ptr(), bare.as_ptr().cast(), bare.len())
            };
            if known == 0 {
                // alloc: an error
                return Err(Unhandled::Llvm(format!(
                    "LLVM's {name} has no target feature {bare:?} (of {:?})",
                    features.names()
                )));
            }
        }
        Ok(machine)
    }
}

/// What a function or data object the code refers to is called in the module, and so in
/// the relocations of the object: the request with that index. No symbol of a program has
/// such a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Symbol {
    Func(u32),
    Data(u32),
}

impl Symbol {
    /// Cranelift's `User` names: namespace 0 holds the functions, 1 the data objects.
    fn of_user(namespace: u32, index: u32) -> Symbol {
        match namespace {
            0 => Symbol::Func(index),
            _ => Symbol::Data(index),
        }
    }

    fn parse(name: &str) -> Option<Symbol> {
        if let Some(index) = name.strip_prefix("blaze.f.") {
            return index.parse().ok().map(Symbol::Func);
        }
        name.strip_prefix("blaze.d.")?.parse().ok().map(Symbol::Data)
    }

    /// The name `name` was before LLVM made it unique (`blaze.d.40.1` of `blaze.d.40`), where
    /// it is one of ours that does not [`parse`](Symbol::parse): a second object was given a
    /// name another had.
    fn uniqued(name: &str) -> Option<Symbol> {
        if Symbol::parse(name).is_some() {
            return None;
        }
        let (stem, suffix) = name.rsplit_once('.')?;
        suffix.bytes().all(|it| it.is_ascii_digit()).then(|| Symbol::parse(stem))?
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Symbol::Func(index) => write!(f, "blaze.f.{index}"),
            Symbol::Data(index) => write!(f, "blaze.d.{index}"),
        }
    }
}

impl Symbol {
    /// Its name, as [`fmt::Display`] writes it, for the module's calls that take one.
    fn named(self) -> SymbolName {
        match self {
            Symbol::Func(index) => SymbolName::of("blaze.f.", index),
            Symbol::Data(index) => SymbolName::of("blaze.d.", index),
        }
    }
}

/// A name of the module's own ([`Symbol::named`], [`shown_name`]) on the stack, with a NUL
/// after it: inkwell takes a `&str` that ends in one as the C string it is, and copies one
/// that does not. Only for inkwell: compared with a name without the NUL, it differs.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SymbolName {
    bytes: [u8; SymbolName::CAPACITY],
    len: u8,
}

impl SymbolName {
    /// A prefix of up to [`SymbolName::PREFIX`] bytes, the ten digits of a `u32` and the NUL.
    const CAPACITY: usize = SymbolName::PREFIX + 10 + 1;
    const PREFIX: usize = 13;

    /// `prefix` and `index` in decimal, as `format!("{prefix}{index}")` writes them.
    fn of(prefix: &str, mut index: u32) -> SymbolName {
        assert!(prefix.len() <= SymbolName::PREFIX, "a prefix too long for a symbol name");
        let mut digits = [0u8; 10];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (index % 10) as u8;
            count += 1;
            index /= 10;
            if index == 0 {
                break;
            }
        }
        let mut bytes = [0u8; SymbolName::CAPACITY];
        let start = prefix.len();
        bytes[..start].copy_from_slice(prefix.as_bytes());
        for (at, &digit) in digits[..count].iter().rev().enumerate() {
            bytes[start + at] = digit;
        }
        SymbolName { bytes, len: (start + count + 1) as u8 }
    }

    /// The name without its NUL, as an object file has it.
    fn bare(&self) -> &str {
        &(**self)[..usize::from(self.len) - 1]
    }
}

impl std::ops::Deref for SymbolName {
    type Target = str;

    fn deref(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("an ASCII name")
    }
}

/// Declares each of `readonly` that holds no function's address with what it holds, its
/// bytes and the addresses into the others, as an `available_externally` constant, as rustc's
/// constants are known to LLVM: a load through a pointer to it folds (a comparison with a
/// literal is one with an immediate), and LLVM emits no copy, so it keeps its one address.
fn told_constants<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    readonly: &HashMap<u32, &super::ConstBlob, Global>,
    pointer: usize,
) {
    let i8 = context.i8_type();
    let mut told: Vec<_> = readonly
        .iter()
        .filter_map(|(&index, &blob)| {
            let mut relocs: Vec<(usize, u32, i64)> = blob
                .relocs
                .iter()
                .map(|&(offset, target, addend)| (offset as usize, target.as_u32(), addend))
                // alloc: once per module per constant, its relocations sorted by offset
                .collect();
            blaze_ids::radix::sort_copy_by_key(&mut relocs, |it| it.0);
            let fits = relocs.iter().all(|it| readonly.contains_key(&it.1))
                && relocs.windows(2).all(|it| it[0].0 + pointer <= it[1].0)
                && relocs.last().is_none_or(|it| it.0 + pointer <= blob.bytes.len());
            (!blob.bytes.is_empty() && blob.fn_relocs.is_empty() && fits)
                .then_some((index, blob, relocs))
        })
        // alloc: once per module, the constants told
        .collect();
    blaze_ids::radix::sort_by_key(&mut told, |it| it.0);
    let mut globals = Vec::with_capacity(told.len());
    // A field before each address, the address, and one after the last.
    let most = told.iter().map(|(_, _, relocs)| 2 * relocs.len() + 1).max().unwrap_or(0);
    let mut fields: Vec<BasicTypeEnum<'ctx>> = Vec::with_capacity(most);
    for (index, blob, relocs) in &told {
        fields.clear();
        let mut at = 0;
        for &(offset, _, _) in relocs {
            if offset > at {
                fields.push(i8.array_type((offset - at) as u32).into());
            }
            fields.push(context.ptr_type(AddressSpace::default()).into());
            at = offset + pointer;
        }
        if at < blob.bytes.len() {
            fields.push(i8.array_type((blob.bytes.len() - at) as u32).into());
        }
        let global = module.add_global(
            context.struct_type(&fields, true),
            None,
            &Symbol::Data(*index).named(),
        );
        global.set_linkage(Linkage::AvailableExternally);
        global.set_visibility(GlobalVisibility::Hidden);
        global.set_constant(true);
        global.set_alignment(blob.align as u32);
        globals.push(global);
    }
    let mut fields: Vec<BasicValueEnum<'ctx>> = Vec::with_capacity(most);
    for ((_, blob, relocs), global) in std::iter::zip(&told, globals) {
        fields.clear();
        let mut at = 0;
        for &(offset, target, addend) in relocs {
            if offset > at {
                fields.push(context.const_string(&blob.bytes[at..offset], false).into());
            }
            let name = Symbol::Data(target).named();
            let target = module.get_global(&name).unwrap_or_else(|| {
                let declared = module.add_global(i8, None, &name);
                declared.set_linkage(Linkage::External);
                declared.set_visibility(GlobalVisibility::Hidden);
                declared
            });
            let address = target.as_pointer_value();
            let address = match addend {
                0 => address,
                // SAFETY: `address` is a global of `module` and the index an `i64` constant of
                // `context`, so this builds a constant expression owned by `context`; not
                // `inbounds`, so any `addend` is defined.
                _ => unsafe {
                    address.const_gep(i8, &[context.i64_type().const_int(addend as u64, true)])
                },
            };
            fields.push(address.into());
            at = offset + pointer;
        }
        if at < blob.bytes.len() {
            fields.push(context.const_string(&blob.bytes[at..], false).into());
        }
        global.set_initializer(&context.const_struct(&fields, true));
    }
}

/// Gives back the bytes of each constant of [`told_constants`] that holds no address, which
/// LLVM's optimizer takes from an `available_externally` object once it is done inlining:
/// the code generator expands a `memcmp` of a few bytes after that, and folds a load of
/// what it knows into an immediate.
fn retold_bytes<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    readonly: &HashMap<u32, &super::ConstBlob, Global>,
) {
    for (&index, blob) in readonly {
        if !blob.points_nowhere() || blob.bytes.is_empty() {
            continue;
        }
        let Some(global) = module.get_global(&Symbol::Data(index).named()) else { continue };
        if global.get_initializer().is_some() {
            continue;
        }
        let bytes = context.const_struct(&[context.const_string(&blob.bytes, false).into()], true);
        if global.get_value_type() != bytes.get_type().as_any_type_enum() {
            continue;
        }
        global.set_initializer(&bytes);
        global.set_linkage(Linkage::AvailableExternally);
    }
}

/// Why LLVM did not compile a function.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Unhandled {
    Opcode(Opcode),
    Type(Type),
    /// IR or an object of a kind there is no answer for yet.
    Shape(&'static str),
    /// A relocation type of the object format, by its number.
    Relocation(u32),
    Llvm(String),
    /// A `return_call` (Rust's `become`) that LLVM can't make a `musttail` call of, with what
    /// stops it: LLVM guarantees the tail call only where the callee is called through the
    /// caller's own type and parameter attributes.
    TailCall(String),
    /// Two functions or data objects under one name of the module's own ([`Symbol`]), which
    /// LLVM would tell apart by renaming one to a name no request has: what they are.
    Duplicate(String),
}

impl fmt::Display for Unhandled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unhandled::Opcode(opcode) => write!(f, "opcode `{opcode}`"),
            Unhandled::Type(ty) => write!(f, "type `{ty}`"),
            Unhandled::Shape(what) => f.write_str(what),
            Unhandled::Relocation(kind) => write!(f, "relocation of type {kind}"),
            // The first line names the problem; the rest names the function's values.
            Unhandled::Llvm(message) => {
                write!(f, "llvm: {}", message.lines().next().unwrap_or_default())
            }
            Unhandled::TailCall(why) => {
                write!(f, "a tail call (`become`) LLVM can't guarantee: {why}")
            }
            Unhandled::Duplicate(what) => {
                write!(f, "two objects under one name of the module's own: {what}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use inkwell::{memory_buffer::MemoryBuffer, passes::PassBuilderOptions, values::AnyValue};

    use super::*;

    #[cfg(not(feature = "llvm-wasm"))]
    #[test]
    fn wasm32_is_refused_by_name_while_blazes_llvm_has_no_webassembly() {
        let wasm32 = crate::Target::of("wasm32-wasip1").unwrap();
        match Machine::create(wasm32, Optimization::NONE) {
            Err(Unhandled::Llvm(what)) => {
                assert!(what.contains("wasm32") && what.contains("llvm-wasm"), "{what}")
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a target machine from an LLVM without WebAssembly"),
        }
    }

    /// Reduced with `llvm-reduce` from a function of bun 1.4.3 at `-O3` on AArch64, where one
    /// run of instruction combining after `default<O3>` leaves more to combine.
    const SHORT_OF_A_FIXPOINT: &str = r#"
define void @f(ptr %0, i1 %.not) personality ptr null {
frame:
  %1 = load volatile i32, ptr null, align 4
  %2 = icmp ne i32 %1, 0
  %.not7.not.not = select i1 %.not, i1 false, i1 %2
  br i1 %.not7.not.not, label %3, label %common.ret

3:
  %4 = load i32, ptr %0, align 1
  %5 = udiv i32 %1, %4
  %6 = icmp eq i32 %5, 1
  call void @llvm.assume(i1 %6)
  %7 = urem i32 %1, %4
  %8 = icmp ne i32 %7, 0
  call void @llvm.assume(i1 %8)
  br label %common.ret

common.ret:
  ret void
}

declare void @llvm.assume(i1 noundef)
"#;

    /// An export that calls itself and its request 1, and reads its datum 0.
    const EXPORTED: &str = r#"
@"blaze.d.0" = external global i32

define i32 @"blaze.own"(i32 %n) {
  %stop = icmp eq i32 %n, 0
  br i1 %stop, label %done, label %more
more:
  %less = sub i32 %n, 1
  %rest = call i32 @"blaze.f.0"(i32 %less)
  ret i32 %rest
done:
  %read = load i32, ptr @"blaze.d.0"
  %other = call i32 @"blaze.f.1"(i32 %read)
  ret i32 %other
}

declare i32 @"blaze.f.0"(i32)
declare i32 @"blaze.f.1"(i32)
"#;

    /// An export that writes through an address it has as an integer, by its request 1.
    const PASSES_AN_ADDRESS: &str = r#"
define void @"blaze.own"(i64 %to, ptr %from) {
  call void @"blaze.f.1"(i64 %to, ptr nonnull %from)
  ret void
}

declare void @"blaze.f.1"(i64, ptr)
"#;

    /// `ptr::write`, which has the address as a pointer, and that of what it writes as an
    /// integer.
    const WRITES: &str = r#"
define void @"blaze.own"(ptr %to, i64 %from) {
  %address = inttoptr i64 %from to ptr
  %value = load i64, ptr %address
  store i64 %value, ptr %to
  ret void
}
"#;

    fn exported(context: &Context, text: &str, funcs: Vec<u32>, id: u32) -> Import {
        let export = super::super::imports::Export {
            bitcode: parsed(context, text).write_bitcode_to_memory().as_slice().into(),
            requests: Default::default(),
            signature: ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV),
            size: 3,
            hint: Hint::Inline,
        };
        Import { export: std::sync::Arc::new(export), id, funcs, datas: Vec::new() }
    }

    /// bun's parser wrote garbage for a `for … of`: `ptr::write::<ForOf>` was imported
    /// through a function that passed it the address as an integer, and LLVM, given a call
    /// of a type the body did not have, kept nothing of what was written.
    #[test]
    fn a_body_imported_through_another_is_called_with_its_own_type() {
        let context = Context::create();
        let passes = exported(&context, PASSES_AN_ADDRESS, vec![5, 6], 5);
        let writes = exported(&context, WRITES, vec![6], 6);
        let module = context.create_module("blaze");
        let (i64, ptr) = (context.i64_type(), context.ptr_type(Default::default()));
        let own = module.add_function(
            OWN,
            context.void_type().fn_type(&[i64.into(), ptr.into()], false),
            None,
        );
        link(&context, &module, &[passes, writes], Linkage::AvailableExternally).unwrap();
        agree_calls(&context, &module).unwrap();
        let builder = context.create_builder();
        builder.position_at_end(context.append_basic_block(own, ""));
        let callee = module.get_function("blaze.f.5").unwrap();
        let args = [own.get_nth_param(0).unwrap().into(), own.get_nth_param(1).unwrap().into()];
        builder.build_direct_call(callee, &args, "").unwrap();
        builder.build_return(None).unwrap();
        module.verify().unwrap();
        let body = module.get_function("blaze.f.5").unwrap();
        let text = body.print_to_string().to_string_lossy().into_owned();
        let all = module.print_to_string().to_string_lossy().into_owned();
        let agreed =
            ["inttoptr i64 %to to ptr", "ptrtoint ptr %from to i64", "@blaze.f.6(ptr %1, i64 %2)"];
        assert!(agreed.iter().all(|it| text.contains(it)), "{all}");

        let optimization =
            Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
        let machine = Machine::create(crate::Target::for_host(), optimization).unwrap();
        let pipeline = optimization.pipeline().unwrap();
        module.run_passes(pipeline, &machine, PassBuilderOptions::create()).unwrap();
        module.run_passes("elim-avail-extern", &machine, PassBuilderOptions::create()).unwrap();
        let text = own.print_to_string().to_string_lossy().into_owned();
        assert!(text.contains("load i64, ptr %1") && text.contains("store i64"), "{text}");
    }

    fn parsed<'ctx>(context: &'ctx Context, text: &str) -> Module<'ctx> {
        let text = format!("{text}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "test");
        context.create_module_from_ir(source).unwrap()
    }

    #[test]
    fn an_import_is_of_the_compiles_requests_and_built_in() {
        let context = Context::create();
        let bitcode: std::sync::Arc<[u8]> =
            parsed(&context, EXPORTED).write_bitcode_to_memory().as_slice().into();
        let export = super::super::imports::Export {
            bitcode,
            requests: Default::default(),
            signature: ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV),
            size: 8,
            hint: Hint::Inline,
        };
        // The export's request 0 is itself; its own 1 is the compile's 7, its datum 0 the 3.
        let import = Import {
            export: std::sync::Arc::new(export),
            id: 5,
            funcs: vec![5, 7],
            datas: vec![3],
        };
        // As `emit` has it: the function compiled is added, what it imports linked in, and
        // its body, which calls the import, lowered.
        let module = context.create_module("blaze");
        let i32 = context.i32_type();
        let own = module.add_function(OWN, i32.fn_type(&[], false), None);
        link(&context, &module, std::slice::from_ref(&import), Linkage::AvailableExternally)
            .unwrap();
        let body = module.get_function("blaze.f.5").expect("the import, linked in");
        assert_eq!(body.get_linkage(), Linkage::AvailableExternally);
        let builder = context.create_builder();
        builder.position_at_end(context.append_basic_block(own, ""));
        let called = builder.build_direct_call(body, &[i32.const_zero().into()], "").unwrap();
        let returned = called.try_as_basic_value().basic().unwrap();
        builder.build_return(Some(&returned)).unwrap();
        module.verify().unwrap();
        assert!(module.get_function("blaze.f.7").is_some_and(|it| it.count_basic_blocks() == 0));
        assert!(module.get_global("blaze.d.3").is_some());
        let names: Vec<String> =
            module.get_functions().map(|it| it.get_name().to_string_lossy().into_owned()).collect();
        assert!(
            !names
                .iter()
                .any(|it| it.contains(".f.0") || it.contains(".f.1") || it.contains("import")),
            "{names:?}"
        );
        let text = body.print_to_string().to_string_lossy().into_owned();
        assert!(text.contains("call i32 @blaze.f.5(i32"), "{text}");

        let optimization =
            Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
        let machine = Machine::create(crate::Target::for_host(), optimization).unwrap();
        let pipeline = optimization.pipeline().unwrap();
        module.run_passes(pipeline, &machine, PassBuilderOptions::create()).unwrap();
        module.run_passes("elim-avail-extern", &machine, PassBuilderOptions::create()).unwrap();
        // Built in, the call is of the other function with the datum, and the body is gone.
        let own = module.get_function(OWN).unwrap();
        let text = own.print_to_string().to_string_lossy().into_owned();
        assert!(text.contains("@blaze.f.7(") && text.contains("@blaze.d.3"), "{text}");
        assert!(!text.contains("blaze.f.5"), "{text}");
        assert!(module.get_function("blaze.f.5").is_none_or(|it| it.count_basic_blocks() == 0));
    }

    /// An export that reads two of its data objects.
    const READS_TWO: &str = r#"
@"blaze.d.0" = external hidden global i8
@"blaze.d.1" = external hidden global i8

define i32 @"blaze.own"() {
  %a = load i32, ptr @"blaze.d.0"
  %b = load i32, ptr @"blaze.d.1"
  %sum = add i32 %a, %b
  ret i32 %sum
}
"#;

    /// [`READS_TWO`] with the two defined, differently.
    const DEFINES_TWO: &str = r#"
@"blaze.d.0" = available_externally hidden constant i32 1
@"blaze.d.1" = available_externally hidden constant i32 2

define i32 @"blaze.own"() {
  %a = load i32, ptr @"blaze.d.0"
  %b = load i32, ptr @"blaze.d.1"
  %sum = add i32 %a, %b
  ret i32 %sum
}
"#;

    /// Two data objects of an export that the compile has as one request (`v2::release`'s
    /// `merge` takes two equal requests as one) are one global of the module, named for the
    /// request: LLVM had called the second `blaze.d.3.1`, which the code then referred to and
    /// nothing defined ("undefined symbol `blaze.d.40.1`" linking build scripts in a cold
    /// self-build, 2026-11-02). Built twice, the object is the same bytes, and its symbols
    /// name the request once.
    #[test]
    fn two_objects_of_an_import_that_are_one_request_are_one_global() {
        let object = || {
            let context = Context::create();
            let mut import = exported(&context, READS_TWO, vec![5], 5);
            import.datas = vec![3, 3];
            let module = context.create_module("blaze");
            let machine = Machine::create(crate::Target::for_host(), Optimization::NONE).unwrap();
            module.set_triple(&machine.get_triple());
            module.set_data_layout(&machine.get_target_data().get_data_layout());
            link(&context, &module, std::slice::from_ref(&import), Linkage::External).unwrap();
            module.verify().unwrap();
            let globals: Vec<String> = module
                .get_globals()
                .map(|it| it.get_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(globals, ["blaze.d.3"], "{}", module.print_to_string().to_string_lossy());
            let body = module
                .get_function("blaze.f.5")
                .unwrap()
                .print_to_string()
                .to_string_lossy()
                .into_owned();
            assert_eq!(body.matches("ptr @blaze.d.3").count(), 2, "{body}");
            machine.write_to_memory_buffer(&module, FileType::Object).unwrap().as_slice().to_vec()
        };
        let (first, second) = (object(), object());
        assert_eq!(first, second, "the object differs between two builds");
        use object::{Object as _, ObjectSymbol as _};
        let file = object::File::parse(&*first).unwrap();
        // Mach-O puts `_` before every C name.
        let mut named: Vec<&str> = file
            .symbols()
            .filter_map(|it| it.name().ok())
            .map(|it| it.strip_prefix('_').unwrap_or(it))
            .filter(|it| it.starts_with("blaze.d."))
            .collect(); // alloc: test
        named.sort_unstable();
        assert_eq!(named, ["blaze.d.3"]);
    }

    /// Two objects of an export that are one request of the compile and defined differently
    /// are refused, naming both and the request, never one renamed.
    #[test]
    fn two_definitions_of_one_request_in_an_import_are_refused_by_name() {
        let context = Context::create();
        let mut import = exported(&context, DEFINES_TWO, vec![5], 5);
        import.datas = vec![3, 3];
        let module = context.create_module("blaze");
        let refused =
            link(&context, &module, std::slice::from_ref(&import), Linkage::External).unwrap_err();
        let Unhandled::Duplicate(what) = &refused else { panic!("{refused:?}") };
        assert!(
            ["`blaze.f.5`", "`blaze.d.0`", "`blaze.d.1`", "`blaze.d.3`"]
                .iter()
                .all(|it| what.contains(it)),
            "{what}"
        );
    }

    /// A reference in the code to a name of the module's own that LLVM made unique is
    /// refused, never taken for an outside symbol of that name.
    #[test]
    fn a_name_llvm_made_unique_is_a_duplicate_and_no_symbol() {
        assert_eq!(Symbol::uniqued("blaze.d.40.1"), Some(Symbol::Data(40)));
        assert_eq!(Symbol::uniqued("blaze.f.7.12"), Some(Symbol::Func(7)));
        assert_eq!(Symbol::uniqued("blaze.d.40"), None);
        assert_eq!(Symbol::uniqued("memcpy"), None);
        assert_eq!(Symbol::uniqued("blaze.d.x.1"), None);
    }

    /// The imports of a compile are linked to each other before they are linked into its
    /// module, and each is there, only available externally, however many there are.
    #[test]
    fn every_import_is_linked_in_available_externally() {
        let context = Context::create();
        let imports: Vec<Import> =
            (4..7).map(|id| exported(&context, WRITES, vec![id], id)).collect();
        let module = context.create_module("blaze");
        link(&context, &module, &imports, Linkage::AvailableExternally).unwrap();
        module.verify().unwrap();
        for id in 4..7 {
            let body = module.get_function(&format!("blaze.f.{id}")).unwrap();
            assert_eq!(body.get_linkage(), Linkage::AvailableExternally);
            assert!(body.count_basic_blocks() > 0);
        }
    }

    /// Two functions compiled as one module come back each as its own code: the call of one
    /// by the other names the callee's request, and the constant both load is one object.
    #[test]
    fn a_unit_reads_back_each_function_with_its_calls_and_constants() {
        reads_back_a_unit_of_two(crate::Target::for_host());
    }

    /// The unit of [`a_unit_reads_back_each_function_with_its_calls_and_constants`] for each
    /// object format: ELF has each function in a section of its own by a COMDAT, Mach-O,
    /// which has no COMDATs (LLVM ends the process on one), in `__text` by its symbol.
    #[test]
    fn a_unit_reads_back_from_mach_o_and_elf_alike() {
        for triple in
            ["aarch64-apple-darwin", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]
        {
            reads_back_a_unit_of_two(crate::Target::of(triple).unwrap());
        }
    }

    /// A COMDAT for a format without them is an error naming the function and the target,
    /// before LLVM is given it.
    #[test]
    fn a_comdat_on_mach_o_is_refused_by_name() {
        let context = Context::create();
        let module = context.create_module("blaze");
        let function =
            module.add_function("blaze.f.3", context.void_type().fn_type(&[], false), None);
        let apple = crate::Target::of("aarch64-apple-darwin").unwrap();
        let refused = own_comdat(&module, apple, function).unwrap_err().to_string();
        assert!(
            refused.contains("blaze.f.3") && refused.contains("aarch64-apple-darwin"),
            "{refused}"
        );
        assert!(function.as_global_value().get_comdat().is_none());
        own_comdat(&module, crate::Target::x86_64_linux(), function).unwrap();
        assert!(function.as_global_value().get_comdat().is_some());
    }

    fn reads_back_a_unit_of_two(target: &'static crate::Target) {
        use cranelift_codegen::ir::{AbiParam, InstBuilder, UserFuncName, types::F64};
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
        use cranelift_module::FuncId;

        use super::super::fragment::{FuncTarget, RelocName};

        let isa = super::super::isa_for(target).unwrap();
        let mut wants = Wants::new(target, &**isa);
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.push(AbiParam::new(F64));
        sig.returns.push(AbiParam::new(F64));
        let callee = wants.func(FuncTarget::Symbol("callee".into()), &sig);
        let caller = wants.func(FuncTarget::Symbol("caller".into()), &sig);
        let mut context = FunctionBuilderContext::new();
        let mut body = |call: Option<FuncId>| {
            let mut func = ir::Function::with_name_signature(UserFuncName::default(), sig.clone());
            let callee = call.map(|it| wants.declare_func_in_func(it, &mut func));
            let mut builder = FunctionBuilder::new(&mut func, &mut context);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            builder.seal_block(block);
            let mut value = builder.block_params(block)[0];
            if let Some(callee) = callee {
                let call = builder.ins().call(callee, &[value]);
                value = builder.inst_results(call)[0];
            }
            // Not an immediate of AArch64's `fmov`, so loaded from memory there too.
            let tenth = builder.ins().f64const(0.1);
            let value = builder.ins().fmul(value, tenth);
            builder.ins().return_(&[value]);
            builder.finalize(isa.frontend_config());
            func
        };
        let (calls, called) = (body(Some(callee)), body(None));
        let members = [
            Member { func: &calls, named: &[], pointees: &[], request: caller.as_u32() },
            Member { func: &called, named: &[], pointees: &[], request: callee.as_u32() },
        ];
        let unit = emit_unit(&mut wants, &members, Optimization::NONE, &[]).unwrap();
        let mut codes: Vec<Code> = unit.members.into_iter().map(Result::unwrap).collect();
        // Of a Mach-O unit each function is its piece of `__text`, and its unwind entry is
        // its own: as long as that piece.
        if target.object_format == crate::ObjectFormat::MachO {
            for code in &codes {
                let unwind = format!("{:?}", code.unwind.as_ref().expect("an unwind entry"));
                assert!(unwind.contains(&format!("len: {},", code.bytes.len())), "{unwind}");
            }
        }
        // The distinct targets of one namespace: AArch64 loads a constant with `adrp` and
        // `ldr`, two relocations of one symbol.
        let named = |code: &Code, wanted| -> Vec<u32> {
            let indices = code.relocs.iter().filter_map(|it| match (it.name, wanted) {
                (RelocName::Func(index), 0) | (RelocName::Data(index), 1) => Some(index),
                _ => None,
            });
            let mut indices: Vec<u32> = indices.collect();
            indices.sort_unstable();
            indices.dedup();
            indices
        };
        assert_eq!(named(&codes[0], 0), [callee.as_u32()]);
        assert_eq!(named(&codes[1], 0), Vec::<u32>::new());
        let constant = named(&codes[0], 1);
        assert_eq!(constant.len(), 1);
        assert_eq!(named(&codes[1], 1), constant);

        let requests = wants.referenced(&mut codes[0]);
        let targets: Vec<_> = requests.funcs.iter().map(|it| &it.target).collect();
        assert!(matches!(&targets[..], [FuncTarget::Symbol(it)] if it == "callee"));
        assert_eq!(requests.datas.len(), 1);
        let requests = wants.referenced(&mut codes[1]);
        assert!(requests.funcs.is_empty());
        assert_eq!(requests.datas.len(), 1);
    }

    /// What a unit imports and LLVM leaves a call of is kept, and read out as code of its
    /// own; what LLVM built into every call is not.
    #[test]
    fn a_unit_keeps_an_import_it_still_calls() {
        use cranelift_codegen::ir::{AbiParam, InstBuilder, UserFuncName, types::F64};
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
        use cranelift_module::FuncId;

        use super::super::fragment::FuncTarget;

        const NOT_BUILT_IN: &str = r#"
define double @"blaze.own"(double %x) #0 {
  %y = fmul double %x, 3.0
  ret double %y
}
attributes #0 = { noinline }
"#;
        const BUILT_IN: &str = r#"
define double @"blaze.own"(double %x) #0 {
  %y = fadd double %x, 2.0
  ret double %y
}
attributes #0 = { alwaysinline }
"#;
        let isa = super::super::isa_for(crate::Target::for_host()).unwrap();
        let mut wants = Wants::new(crate::Target::for_host(), &**isa);
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.push(AbiParam::new(F64));
        sig.returns.push(AbiParam::new(F64));
        let kept = wants.func(FuncTarget::Symbol("kept".into()), &sig);
        let built_in = wants.func(FuncTarget::Symbol("built in".into()), &sig);
        let caller = wants.func(FuncTarget::Symbol("caller".into()), &sig);
        let mut func = ir::Function::with_name_signature(UserFuncName::default(), sig.clone());
        let calls = [kept, built_in].map(|it| wants.declare_func_in_func(it, &mut func));
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut context);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        builder.seal_block(block);
        let mut value = builder.block_params(block)[0];
        for callee in calls {
            let call = builder.ins().call(callee, &[value]);
            value = builder.inst_results(call)[0];
        }
        builder.ins().return_(&[value]);
        builder.finalize(isa.frontend_config());

        let llvm = Context::create();
        let import = |text, id: FuncId| Import {
            export: std::sync::Arc::new(super::super::imports::Export {
                bitcode: parsed(&llvm, text).write_bitcode_to_memory().as_slice().into(),
                requests: Default::default(),
                signature: sig.clone(),
                size: 2,
                hint: Hint::None,
            }),
            id: id.as_u32(),
            funcs: vec![id.as_u32()],
            datas: Vec::new(),
        };
        let imports = [import(NOT_BUILT_IN, kept), import(BUILT_IN, built_in)];
        let members = [Member { func: &func, named: &[], pointees: &[], request: caller.as_u32() }];
        let optimization =
            Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
        let unit = emit_unit(&mut wants, &members, optimization, &imports).unwrap();
        let mut code = unit.members.into_iter().next().unwrap().unwrap();
        let requests = wants.referenced(&mut code);
        let targets: Vec<_> = requests.funcs.iter().map(|it| &it.target).collect();
        assert!(matches!(&targets[..], [FuncTarget::Symbol(it)] if it == "kept"));
        let kept: Vec<u32> = unit.kept.iter().map(|it| it.0).collect();
        assert_eq!(kept, [imports[0].id]);
        assert!(unit.kept[0].1.as_ref().is_ok_and(|it| !it.bytes.is_empty()));
    }

    /// LLVM's assembler leaves out an instruction it cannot encode and writes the object all
    /// the same; that is an error here, not code missing an instruction.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn assembly_llvm_cannot_encode_is_an_error() {
        let refused = assemble(crate::Target::for_host(), "mov ah, sil\nret").unwrap_err();
        assert!(refused.contains("can't encode 'ah'"), "{refused}");
    }

    /// A function LLVM has no lowering for is an error the build stops on, naming what LLVM
    /// lacks, never a function left to Cranelift.
    #[test]
    fn a_function_llvm_cannot_lower_is_refused() {
        use cranelift_codegen::ir::{AbiParam, InstBuilder, UserFuncName, types::I64};
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

        let isa = super::super::isa_for(crate::Target::for_host()).unwrap();
        let mut wants = Wants::new(crate::Target::for_host(), &**isa);
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.returns.push(AbiParam::new(I64));
        let mut func = ir::Function::with_name_signature(UserFuncName::default(), sig);
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut context);
        let block = builder.create_block();
        builder.switch_to_block(block);
        builder.seal_block(block);
        let stack = builder.ins().get_stack_pointer(I64);
        builder.ins().return_(&[stack]);
        builder.finalize(isa.frontend_config());
        let Err(refused) = emit(&mut wants, &[], &func, Optimization::NONE, None, &[]) else {
            panic!("`get_stack_pointer` was emitted");
        };
        assert_eq!(refused, Unhandled::Opcode(Opcode::GetStackPointer));
        let error = super::super::CodegenError::LlvmRefused(refused);
        assert!(error.to_string().contains("get_stack_pointer"), "{error}");
    }

    /// Every instruction on vectors the front end makes (`neon.rs` and `x86.rs` for the
    /// `core::arch` intrinsics a vector instruction is, `wasm_simd.rs` for the generic `simd_*`
    /// ones, on every target) is one LLVM lowers, as the stencils do: a function of each, its
    /// operands loaded and its result stored as the front end does, compiles. `vpadalq_u8`
    /// (`uwiden_low`), `vqtbl1q_u8` (`swizzle`) and `vcombine_u64` (`scalar_to_vector`) were
    /// refused at release, so code using NEON did not build (sb-simd, 2026-11-02).
    #[test]
    fn every_vector_instruction_the_front_end_makes_is_lowered_by_llvm() {
        use cranelift_codegen::ir::{
            AbiParam, InstBuilder, UserFuncName, Value, condcodes::FloatCC, types::*,
        };
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

        type Make = fn(&mut FunctionBuilder<'_>, Value) -> Value;
        fn at(b: &mut FunctionBuilder<'_>, ty: ir::Type, p: Value, offset: i32) -> Value {
            b.ins().load(ty, crate::MEM, p, offset)
        }
        let cases: &[(&str, Make)] = &[
            ("swizzle", |b, p| {
                let (x, i) = (at(b, I8X16, p, 0), at(b, I8X16, p, 16));
                b.ins().swizzle(x, i)
            }),
            ("uwiden_low", |b, p| {
                let x = at(b, I8X16, p, 0);
                b.ins().uwiden_low(x)
            }),
            ("uwiden_high", |b, p| {
                let x = at(b, I16X8, p, 0);
                b.ins().uwiden_high(x)
            }),
            ("swiden_low", |b, p| {
                let x = at(b, I32X4, p, 0);
                b.ins().swiden_low(x)
            }),
            ("swiden_high", |b, p| {
                let x = at(b, I8X16, p, 0);
                b.ins().swiden_high(x)
            }),
            ("scalar_to_vector", |b, p| {
                let x = at(b, I64, p, 0);
                b.ins().scalar_to_vector(I64X2, x)
            }),
            ("scalar_to_vector f32", |b, p| {
                let x = at(b, F32, p, 0);
                b.ins().scalar_to_vector(F32X4, x)
            }),
            ("avg_round", |b, p| {
                let (x, y) = (at(b, I8X16, p, 0), at(b, I8X16, p, 16));
                b.ins().avg_round(x, y)
            }),
            ("sqmul_round_sat", |b, p| {
                let (x, y) = (at(b, I16X8, p, 0), at(b, I16X8, p, 16));
                b.ins().sqmul_round_sat(x, y)
            }),
            ("snarrow", |b, p| {
                let (x, y) = (at(b, I16X8, p, 0), at(b, I16X8, p, 16));
                b.ins().snarrow(x, y)
            }),
            ("unarrow", |b, p| {
                let (x, y) = (at(b, I32X4, p, 0), at(b, I32X4, p, 16));
                b.ins().unarrow(x, y)
            }),
            ("uunarrow", |b, p| {
                let (x, y) = (at(b, I64X2, p, 0), at(b, I64X2, p, 16));
                b.ins().uunarrow(x, y)
            }),
            ("iabs", |b, p| {
                let x = at(b, I8X16, p, 0);
                b.ins().iabs(x)
            }),
            ("iabs scalar", |b, p| {
                let x = at(b, I32, p, 0);
                b.ins().iabs(x)
            }),
            ("popcnt", |b, p| {
                let x = at(b, I8X16, p, 0);
                b.ins().popcnt(x)
            }),
            ("fadd", |b, p| {
                let (x, y) = (at(b, F32X4, p, 0), at(b, F32X4, p, 16));
                b.ins().fadd(x, y)
            }),
            ("fdiv", |b, p| {
                let (x, y) = (at(b, F64X2, p, 0), at(b, F64X2, p, 16));
                b.ins().fdiv(x, y)
            }),
            ("fmin", |b, p| {
                let (x, y) = (at(b, F32X4, p, 0), at(b, F32X4, p, 16));
                b.ins().fmin(x, y)
            }),
            ("fneg", |b, p| {
                let x = at(b, F64X2, p, 0);
                b.ins().fneg(x)
            }),
            ("sqrt", |b, p| {
                let x = at(b, F32X4, p, 0);
                b.ins().sqrt(x)
            }),
            ("nearest", |b, p| {
                let x = at(b, F32X4, p, 0);
                b.ins().nearest(x)
            }),
            ("fcmp", |b, p| {
                let (x, y) = (at(b, F32X4, p, 0), at(b, F32X4, p, 16));
                b.ins().fcmp(FloatCC::LessThan, x, y)
            }),
            ("bitselect", |b, p| {
                let (c, x, y) = (at(b, I8X16, p, 0), at(b, I8X16, p, 16), at(b, I8X16, p, 32));
                b.ins().bitselect(c, x, y)
            }),
            ("bitselect f32", |b, p| {
                let (c, x, y) = (at(b, F32X4, p, 0), at(b, F32X4, p, 16), at(b, F32X4, p, 32));
                b.ins().bitselect(c, x, y)
            }),
            ("fcvt_from_sint", |b, p| {
                let x = at(b, I32X4, p, 0);
                b.ins().fcvt_from_sint(F32X4, x)
            }),
            ("fcvt_from_uint", |b, p| {
                let x = at(b, I32X4, p, 0);
                b.ins().fcvt_from_uint(F32X4, x)
            }),
            ("fcvt_to_sint_sat", |b, p| {
                let x = at(b, F32X4, p, 0);
                b.ins().fcvt_to_sint_sat(I32X4, x)
            }),
            ("fcvt_to_uint_sat", |b, p| {
                let x = at(b, F32X4, p, 0);
                b.ins().fcvt_to_uint_sat(I32X4, x)
            }),
            ("fvpromote_low", |b, p| {
                let x = at(b, F32X4, p, 0);
                b.ins().fvpromote_low(x)
            }),
            ("fvdemote", |b, p| {
                let x = at(b, F64X2, p, 0);
                b.ins().fvdemote(x)
            }),
            ("uload8x8", |b, p| b.ins().uload8x8(crate::MEM, p, 0)),
            ("sload32x2", |b, p| b.ins().sload32x2(crate::MEM, p, 8)),
            ("umulhi", |b, p| {
                let (x, y) = (at(b, I16X8, p, 0), at(b, I16X8, p, 16));
                b.ins().umulhi(x, y)
            }),
            ("vconst", |b, _| {
                let constant = b.func.dfg.constants.insert((0..16).collect::<Vec<u8>>().into());
                b.ins().vconst(I32X4, constant)
            }),
        ];
        let isa = super::super::isa_for(crate::Target::for_host()).unwrap();
        let mut refused = Vec::new();
        for (name, make) in cases {
            let mut wants = Wants::new(crate::Target::for_host(), &**isa);
            let mut sig = ir::Signature::new(isa.default_call_conv());
            sig.params.push(AbiParam::new(I64));
            let mut func = ir::Function::with_name_signature(UserFuncName::default(), sig);
            let mut context = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut func, &mut context);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            builder.seal_block(block);
            let p = builder.block_params(block)[0];
            let value = make(&mut builder, p);
            builder.ins().store(crate::MEM, value, p, 64);
            builder.ins().return_(&[]);
            builder.finalize(isa.frontend_config());
            for optimization in [
                Optimization::NONE,
                Optimization { passes: Passes::Aggressive, selection: Selection::Optimized },
            ] {
                if let Err(why) = emit(&mut wants, &[], &func, optimization, None, &[]) {
                    refused.push(format!("{name}: {why}"));
                }
            }
        }
        assert!(refused.is_empty(), "LLVM refused:\n{}", refused.join("\n"));
    }

    /// A target machine is told its set in LLVM's names, one per set a thread compiles with,
    /// and a name LLVM's target lacks is an error naming it, not a machine of fewer features.
    #[test]
    fn a_machine_is_told_its_features_and_refuses_one_llvm_lacks() {
        let target = crate::Target::for_host();
        let wanted = match target.arch {
            Arch::X86_64 => "+avx2",
            Arch::Aarch64 => "+dotprod",
            Arch::Wasm32 | Arch::Wasm64 => return,
        };
        let set = target.features_of(target.cpu(), &[wanted.to_owned()]).unwrap();
        let machine =
            Machine::create_for(crate::Target::for_host(), Optimization::NONE, set).unwrap();
        let told = |it: &TargetMachine| it.get_feature_string().to_string_lossy().into_owned();
        assert!(told(&machine).contains(wanted), "{}", told(&machine));
        let baseline = Machine::create(crate::Target::for_host(), Optimization::NONE).unwrap();
        assert!(!told(&baseline).contains(wanted), "{}", told(&baseline));
        let unknown = target.features_of(target.cpu(), &["+no-such-feature".to_owned()]).unwrap();
        let Err(Unhandled::Llvm(refused)) =
            Machine::create_for(crate::Target::for_host(), Optimization::NONE, unknown)
        else {
            panic!("a machine told a feature LLVM lacks");
        };
        assert!(refused.contains("no-such-feature"), "{refused}");
        let with_set =
            Machine::with_pgo(crate::Target::for_host(), Optimization::NONE, None, set, |_, it| {
                Ok(told(it))
            })
            .unwrap();
        let without =
            Machine::with(crate::Target::for_host(), Optimization::NONE, |_, it| Ok(told(it)))
                .unwrap();
        assert!(with_set.contains(wanted) && !without.contains(wanted), "{with_set} {without}");
    }

    /// A function of a unit for a CPU past its target's baseline (`-C target-cpu=x86-64-v3`,
    /// `apple-m4`, `neoverse-n1`) is told that CPU's name, as its target machine is; one of the
    /// baseline's the baseline's.
    #[test]
    fn a_function_and_its_machine_are_told_the_units_cpu() {
        use cranelift_codegen::ir::UserFuncName;

        let target = crate::Target::for_host();
        let cpu = match (target.arch, target.object_format) {
            (Arch::X86_64, _) => "x86-64-v3",
            (Arch::Aarch64, crate::ObjectFormat::MachO) => "apple-m4",
            (Arch::Aarch64, _) => "neoverse-n1",
            (Arch::Wasm32 | Arch::Wasm64, _) => return,
        };
        let isa = super::super::isa_for(target).unwrap();
        let signature = ir::Signature::new(isa.default_call_conv());
        let func = ir::Function::with_name_signature(UserFuncName::default(), signature);
        let named = target.features_of(cpu, &[]).unwrap();
        for (features, told) in [(crate::Features::BASELINE, target.cpu()), (named, cpu)] {
            let context = Context::create();
            let module = context.create_module("cpu");
            let own = Defined {
                name: OWN,
                func: &func,
                pointees: &[],
                named: &[],
                weights: None,
                marked: false,
            };
            Lowering::add(&context, &module, target, &own, false, features).unwrap();
            let text = module.print_to_string().to_string();
            assert!(text.contains(&format!("\"target-cpu\"=\"{told}\"")), "{told}: {text}");
            let machine = Machine::create_for(target, Optimization::NONE, features).unwrap();
            assert_eq!(machine.get_cpu().to_string(), told);
        }
    }

    /// What the kept passes make of a module is what LLVM's own run of the pipeline, with
    /// everything built for the one call, makes of it, whatever they ran before.
    #[test]
    fn kept_passes_do_what_passes_built_for_each_run_do() {
        let optimization =
            Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
        let context = Context::create();
        let machinery =
            Machinery::new(Machine::create(crate::Target::for_host(), optimization).unwrap());
        let fresh = Machine::create(crate::Target::for_host(), optimization).unwrap();
        let pipelines = [
            optimization.pipeline().unwrap(),
            Optimization { passes: Passes::Default, ..optimization }.pipeline().unwrap(),
            super::exported(optimization),
            "thinlto<O2>",
            "thinlto<O3>",
            "default<O1>",
            REBASED,
            "elim-avail-extern",
            "cgscc(function-attrs)",
            "cgscc(inline,function(sroa,instsimplify,sroa,jump-threading,early-cse<memssa>)),\
             thinlto<O3>",
        ];
        let samples = [EXPORTED, PASSES_AN_ADDRESS, WRITES, SHORT_OF_A_FIXPOINT];
        for pipeline in pipelines {
            for slp in [false, true] {
                // Twice, so that each sample also runs after every other.
                for _ in 0..2 {
                    for sample in samples {
                        let kept = parsed(&context, sample);
                        machinery.run(&kept, pipeline, slp).unwrap();
                        let alone = parsed(&context, sample);
                        let options = PassBuilderOptions::create();
                        options.set_loop_slp_vectorization(slp);
                        alone.run_passes(pipeline, &fresh, options).unwrap();
                        assert_eq!(
                            kept.print_to_string().to_string(),
                            alone.print_to_string().to_string(),
                            "{pipeline}, slp {slp}"
                        );
                    }
                }
            }
        }
        // A module of another context is refused, not run.
        let other = Context::create();
        assert!(machinery.run(&parsed(&other, WRITES), "thinlto<O2>", false).is_err());
        // A pipeline LLVM cannot parse is an error, and the passes work after it.
        assert!(machinery.run(&parsed(&context, WRITES), "no-such-pass", false).is_err());
        machinery.run(&parsed(&context, WRITES), "thinlto<O2>", false).unwrap();
    }

    #[test]
    fn instruction_combining_short_of_a_fixpoint_does_not_abort() {
        let optimization =
            Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
        let machine = Machine::create(crate::Target::for_host(), optimization).unwrap();
        let context = Context::create();
        // LLVM reads to a nul.
        let text = format!("{SHORT_OF_A_FIXPOINT}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "short");
        let module = context.create_module_from_ir(source).unwrap();
        let pipeline = optimization.pipeline().unwrap();
        module.run_passes(pipeline, &machine, PassBuilderOptions::create()).unwrap();
    }

    /// The safe byte GEPs give the IR inkwell's unsafe `build_gep`/`build_in_bounds_gep` of
    /// `i8` give, which is what the lowering built before they were wrapped: for a parameter
    /// offset, a constant one and a negative one, and the module verifies.
    #[test]
    fn byte_offsets_are_the_geps_of_bytes() {
        let context = Context::create();
        let module = context.create_module("offsets");
        let (pointer, i64) = (context.ptr_type(AddressSpace::default()), context.i64_type());
        let ty = context.void_type().fn_type(&[pointer.into(), i64.into()], false);
        let function = module.add_function("f", ty, None);
        let builder = context.create_builder();
        builder.position_at_end(context.append_basic_block(function, ""));
        let base = function.get_nth_param(0).unwrap().into_pointer_value();
        let by = function.get_nth_param(1).unwrap().into_int_value();
        byte_offset(&builder, &context, base, by).unwrap();
        byte_offset(&builder, &context, base, i64.const_int(-3i64 as u64, true)).unwrap();
        in_bounds_byte_offset(&builder, &context, base, i64.const_int(8, false)).unwrap();
        builder.build_return(None).unwrap();
        module.verify().unwrap();
        let text = function.print_to_string().to_string();
        assert!(text.contains("= getelementptr i8, ptr %0, i64 %1"), "{text}");
        assert!(text.contains("= getelementptr i8, ptr %0, i64 -3"), "{text}");
        assert!(text.contains("= getelementptr inbounds i8, ptr %0, i64 8"), "{text}");
        // Unpositioned, the builder refuses as inkwell's own call does.
        let loose = context.create_builder();
        assert!(byte_offset(&loose, &context, base, by).is_err());
    }

    /// Every function blaze gives LLVM on an Apple target keeps a frame record where it calls,
    /// unforced (v1's units and exports pass `false`), as rustc's `non-leaf`: a release
    /// function that saved x29/x30 below an unmoved x29 left its caller to its DWARF rows
    /// alone, and ld64 encoded those frameless, so a panic on a fiber found no catch
    /// (2026-11-04). Forced, every function keeps one; elsewhere unforced, none is asked.
    #[test]
    fn apple_functions_keep_a_frame_record_unforced() {
        let attribute = |triple: &str, forced: bool| {
            let target = crate::Target::of(triple).expect("a target");
            let func = ir::Function::with_name_signature(
                ir::UserFuncName::default(),
                ir::Signature::new(target.call_conv),
            );
            let context = Context::create();
            let module = context.create_module("frame");
            let own = Defined { name: OWN, func: &func, pointees: &[], named: &[], weights: None, marked: false };
            let function = Lowering::add(&context, &module, target, &own, forced, crate::Features::BASELINE).expect("added");
            function
                .get_string_attribute(AttributeLoc::Function, "frame-pointer")
                .map(|it| it.get_string_value().to_string_lossy().into_owned())
        };
        assert_eq!(attribute("aarch64-apple-darwin", false).as_deref(), Some("non-leaf"));
        assert_eq!(attribute("x86_64-apple-darwin", false).as_deref(), Some("non-leaf"));
        assert_eq!(attribute("aarch64-apple-darwin", true).as_deref(), Some("all"));
        assert_eq!(attribute("aarch64-unknown-linux-gnu", false), None);
        assert_eq!(attribute("x86_64-unknown-linux-gnu", true).as_deref(), Some("all"));
    }

    /// The text of the module `func` lowers to as the function being compiled, whose
    /// parameters point to `pointees`, once it verifies, and once `pipeline` ran over it (and
    /// it verified again) if one is given.
    fn lowered_alone(
        func: &ir::Function,
        pointees: &[(u32, super::super::fragment::Pointee)],
        pipeline: Option<&str>,
    ) -> Result<String, Unhandled> {
        let target = crate::Target::for_host();
        let context = Context::create();
        let module = context.create_module("tail");
        let own = Defined { name: OWN, func, pointees, named: &[], weights: None, marked: false };
        let baseline = crate::Features::BASELINE;
        let function = Lowering::add(&context, &module, target, &own, false, baseline)?;
        let constants: HashMap<u32, &[u8], Global> = HashMap::new_in(Global);
        let statics: HashSet<u32, Global> = HashSet::new_in(Global);
        let blocks_asm: HashMap<u32, &'static crate::asm::Inline, Global> = HashMap::new_in(Global);
        let shown: HashMap<u32, FunctionValue<'_>, Global> = HashMap::new_in(Global);
        Lowering::define(
            &context,
            &module,
            target,
            function,
            &own,
            &constants,
            &statics,
            None,
            None,
            &blocks_asm,
            &shown,
            None,
        )?;
        let verified = |module: &Module<'_>| {
            module.verify().map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))
        };
        verified(&module)?;
        if let Some(pipeline) = pipeline {
            let optimization =
                Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
            let machine = Machine::create(target, optimization)?;
            module.set_triple(&machine.get_triple());
            module.set_data_layout(&machine.get_target_data().get_data_layout());
            Machinery::new(machine).run(&module, pipeline, true)?;
            verified(&module)?;
        }
        Ok(module.print_to_string().to_string())
    }

    /// Each `musttail` call of `text` is followed by a `ret`, and there is one.
    fn musttail_then_ret(text: &str) {
        let lines: Vec<&str> = text.lines().collect();
        let calls = lines.iter().enumerate().filter(|(_, it)| it.contains("musttail call"));
        let mut found = 0;
        for (at, call) in calls {
            found += 1;
            let next = lines.get(at + 1).map_or("", |it| it.trim_start());
            assert!(next.starts_with("ret "), "`{call}` is followed by `{next}`:\n{text}");
        }
        assert!(found > 0, "no `musttail` call:\n{text}");
    }

    /// `n, acc, rest...` in a function of `sig`, all `i64`: `acc` once `n` is 0, else what
    /// `tail` makes of the parameters and of `n - 1, acc + 1, rest...` in the builder, a tail
    /// call.
    fn counting_down(
        sig: &ir::Signature,
        config: cranelift_codegen::isa::TargetFrontendConfig,
        tail: impl FnOnce(&mut cranelift_frontend::FunctionBuilder<'_>, &[ir::Value], &[ir::Value]),
    ) -> ir::Function {
        use cranelift_codegen::ir::{InstBuilder, UserFuncName, types::I64};
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

        let mut func = ir::Function::with_name_signature(UserFuncName::default(), sig.clone());
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut context);
        let (entry, again, done) =
            (builder.create_block(), builder.create_block(), builder.create_block());
        builder.append_block_params_for_function_params(entry);
        builder.append_block_param(done, I64);
        builder.switch_to_block(entry);
        let params = builder.block_params(entry).to_vec();
        builder.ins().brif(params[0], again, &[], done, &[params[1].into()]);
        builder.switch_to_block(again);
        let mut args = params.clone();
        let (less, one) = (builder.ins().iconst(I64, -1), builder.ins().iconst(I64, 1));
        args[0] = builder.ins().iadd(params[0], less);
        args[1] = builder.ins().iadd(params[1], one);
        tail(&mut builder, &params, &args);
        builder.switch_to_block(done);
        let acc = builder.block_params(done)[0];
        builder.ins().return_(&[acc]);
        builder.seal_all_blocks();
        builder.finalize(config);
        func
    }

    /// `become f(n - 1, acc + 1, ...)` in `f` is a `musttail` call of `f`'s request followed by
    /// the `ret` of what it returns, with arguments in registers only and with some on the
    /// stack; LLVM's optimizing pipeline keeps every call of it `musttail`, and the code reads
    /// back with the call as a relocation to the request.
    #[test]
    fn a_tail_call_of_itself_is_musttail_then_ret_with_arguments_on_the_stack_too() {
        use cranelift_codegen::ir::{AbiParam, InstBuilder, types::I64};

        use super::super::fragment::{FuncTarget, RelocName};

        let isa = super::super::isa_for(crate::Target::for_host()).unwrap();
        // Twelve take more than the registers of SysV x86-64 (6) and AArch64 (8).
        for count in [2, 12] {
            let mut wants = Wants::new(crate::Target::for_host(), &**isa);
            let mut sig = ir::Signature::new(isa.default_call_conv());
            sig.params.extend(std::iter::repeat_n(AbiParam::new(I64), count));
            sig.returns.push(AbiParam::new(I64));
            let own = wants.func(FuncTarget::Symbol("counting down".into()), &sig);
            let func = counting_down(&sig, isa.frontend_config(), |builder, _, args| {
                let callee = wants.declare_func_in_func(own, builder.func);
                builder.ins().return_call(callee, args);
            });
            let text = lowered_alone(&func, &[], None).unwrap();
            musttail_then_ret(&text);
            let optimized = optimized_pipeline();
            let text = lowered_alone(&func, &[], Some(optimized)).unwrap();
            let request = Symbol::Func(own.as_u32()).named();
            let calls = text.lines().filter(|it| it.contains(&format!("@{}(", request.bare())));
            for call in calls.filter(|it| it.contains("call ")) {
                assert!(call.contains("musttail call"), "{count}: `{call}`:\n{text}");
            }
            for optimization in [Optimization::NONE, optimized_level()] {
                let code = emit(&mut wants, &[], &func, optimization, None, &[]).unwrap();
                let calls =
                    code.relocs.iter().filter(|it| it.name == RelocName::Func(own.as_u32()));
                assert!(calls.count() > 0, "{count}: {:?}", code.relocs);
            }
        }
    }

    fn optimized_level() -> Optimization {
        Optimization { passes: Passes::Aggressive, selection: Selection::Optimized }
    }

    fn optimized_pipeline() -> &'static str {
        optimized_level().pipeline().unwrap()
    }

    /// A function that returns through `sret` and tail-calls another that does passes its own
    /// `sret` on, with the attribute at the call; a pointer parameter of the caller is passed
    /// an integer and an integer one a pointer, each turned into what the caller has there,
    /// and the whole compile (wrapped on Mach-O, where the pointer is the only way to its
    /// memory) keeps the call.
    #[test]
    fn a_tail_call_passes_on_sret_and_converts_pointers_to_the_callers_types() {
        use cranelift_codegen::ir::{
            AbiParam, ArgumentPurpose, InstBuilder, MemFlagsData, types::I64,
        };

        use super::super::fragment::{FuncTarget, Pointee};

        let isa = super::super::isa_for(crate::Target::for_host()).unwrap();
        let mut wants = Wants::new(crate::Target::for_host(), &**isa);

        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.push(AbiParam::special(I64, ArgumentPurpose::StructReturn));
        sig.params.push(AbiParam::new(I64));
        let other = wants.func(FuncTarget::Symbol("returns through memory".into()), &sig);
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig.clone());
        let callee = wants.declare_func_in_func(other, &mut func);
        {
            let mut context = cranelift_frontend::FunctionBuilderContext::new();
            let mut builder = cranelift_frontend::FunctionBuilder::new(&mut func, &mut context);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            let [to, n] = *builder.block_params(block) else { unreachable!() };
            builder.ins().store(MemFlagsData::new().with_notrap(), n, to, 0);
            let one = builder.ins().iconst(I64, 1);
            let less = builder.ins().isub(n, one);
            builder.ins().return_call(callee, &[to, less]);
            builder.seal_all_blocks();
            builder.finalize(isa.frontend_config());
        }
        let text = lowered_alone(&func, &[], None).unwrap();
        musttail_then_ret(&text);
        let call = text.lines().find(|it| it.contains("musttail call")).unwrap();
        assert!(call.contains("sret(i8)") && call.contains("%0, i64 %"), "{call}");
        assert!(text.contains("ret void"), "{text}");

        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
        sig.returns.push(AbiParam::new(I64));
        let other = wants.func(FuncTarget::Symbol("swapped".into()), &sig);
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig.clone());
        let callee = wants.declare_func_in_func(other, &mut func);
        {
            let mut context = cranelift_frontend::FunctionBuilderContext::new();
            let mut builder = cranelift_frontend::FunctionBuilder::new(&mut func, &mut context);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            let [address, n] = *builder.block_params(block) else { unreachable!() };
            let eight = builder.ins().iconst(I64, 8);
            let past = builder.ins().iadd(address, eight);
            builder.ins().return_call(callee, &[n, past]);
            builder.seal_all_blocks();
            builder.finalize(isa.frontend_config());
        }
        let pointee = Pointee { unique: true, frozen: false, size: 16, align: 8 };
        let text = lowered_alone(&func, &[(0, pointee)], None).unwrap();
        musttail_then_ret(&text);
        assert!(text.contains("define i64 @blaze.own(ptr"), "{text}");
        assert!(text.contains("inttoptr i64 %"), "{text}");
        assert!(text.contains("ptrtoint ptr %"), "{text}");
        let call = text.lines().find(|it| it.contains("musttail call")).unwrap();
        assert!(call.contains("(ptr %") && call.contains(", i64 %"), "{call}");
        wants.pointees = vec![(0, pointee)];
        emit(&mut wants, &[], &func, optimized_level(), None, &[]).unwrap();
    }

    /// `return_call_indirect` is a `musttail` call through the pointer, of the caller's type.
    #[test]
    fn an_indirect_tail_call_is_musttail_then_ret() {
        use cranelift_codegen::ir::{AbiParam, InstBuilder, types::I64};

        let isa = super::super::isa_for(crate::Target::for_host()).unwrap();
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
        sig.returns.push(AbiParam::new(I64));
        // `(n, f)`: `f`, the function itself, is passed on and called.
        let func = counting_down(&sig, isa.frontend_config(), |builder, params, args| {
            let called = builder.import_signature(sig.clone());
            builder.ins().return_call_indirect(called, params[1], &[args[0], params[1]]);
        });
        let text = lowered_alone(&func, &[], None).unwrap();
        musttail_then_ret(&text);
        let call = text.lines().find(|it| it.contains("musttail call")).unwrap();
        assert!(call.contains("musttail call i64 %"), "{call}");
    }

    /// A tail call of a callee that takes other parameters than the caller, as a call through
    /// a vtable does (the receiver one pointer where the caller has two), is refused, naming
    /// both signatures, rather than made a call that grows the stack.
    #[test]
    fn a_tail_call_of_other_parameters_is_refused() {
        use cranelift_codegen::ir::{AbiParam, InstBuilder, types::I64};

        let isa = super::super::isa_for(crate::Target::for_host()).unwrap();
        let mut wants = Wants::new(crate::Target::for_host(), &**isa);
        let mut sig = ir::Signature::new(isa.default_call_conv());
        sig.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
        sig.returns.push(AbiParam::new(I64));
        let mut method = ir::Signature::new(isa.default_call_conv());
        method.params.push(AbiParam::new(I64));
        method.returns.push(AbiParam::new(I64));
        let func = counting_down(&sig, isa.frontend_config(), |builder, params, args| {
            let called = builder.import_signature(method.clone());
            builder.ins().return_call_indirect(called, params[1], &[args[0]]);
        });
        let Err(Unhandled::TailCall(why)) = lowered_alone(&func, &[], None) else {
            panic!("a tail call of one parameter from a function of two was lowered");
        };
        assert!(why.contains("passes 1 parameters") && why.contains("the caller has 2"), "{why}");
        let Err(refused) = emit(&mut wants, &[], &func, Optimization::NONE, None, &[]) else {
            panic!("a tail call of one parameter from a function of two was emitted");
        };
        assert!(matches!(&refused, Unhandled::TailCall(it) if *it == why), "{refused:?}");
        let error = super::super::CodegenError::LlvmRefused(refused).to_string();
        assert!(error.contains("tail call") && error.contains("vtable"), "{error}");
    }
}
