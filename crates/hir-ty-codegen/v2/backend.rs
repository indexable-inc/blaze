//! The backends of a v2 build ([`Prepare`]): the IR builder's IR of an instance against its
//! references by place, compiled with Cranelift, or handed to LLVM ([`super::release`]).

use std::marker::PhantomData;

use base_db::{Crate, Db};
use blaze_codegen::{
    Arch, Backend, CodegenError, Compile, CompiledFunction, Static, Unit,
    backend::{Emitted, FrontRef, Import, Prepare, Shape},
    fold::{Group, Masked, Merged},
    settings::{DebugInfo, Pgo, Relocation, Sanitizers, Spec},
    summary::{Imports, Summary},
    template::Refs,
};
use blaze_format::SpanRef;
use blaze_ids::{ContentId, bits::Bits};
use blaze_items::{CodeHash, ItemId, ItemKey};
use blaze_mem::Frame;
use cranelift_codegen::{ir, isa::TargetIsa};
use hir_def::{
    DefWithBodyId, HasModule,
    attrs::AttrFlags,
    expr_store::{
        Body,
        scope::{ExprScopes, ScopeId},
    },
    hir::BindingId,
    signatures::FunctionSignature,
};
use hir_ty::{ParamEnvAndCrate, db::HirDatabase, next_solver::ParamEnv};
use hir_ty_mir::mir::MirSpan;

use super::{
    Built, DataPlace, Function, HashSource, Hashes, HirFrontend, HirInstance, Kind, Places,
    Prepared,
    build::Build,
    failed,
    places::{Placer, Ref, evaluated, place_requests, resolve},
    refs, shape, side,
};
use crate::{
    Detach, Instance, InstanceDef, Produced, compile_as,
    debuginfo::{LineRow, SlotPlaces, VariablePlace},
    fragment::{self, Hint, Requests},
    frame_map::{self, FrameHashMap},
    instance_code,
    unwind::FunctionUnwind,
};

/// Cranelift and LLVM over the IR builder.
#[derive(Default)]
pub struct Backends<'db>(PhantomData<&'db ()>);

/// Variables of the environment that change the code the IR builder makes, which a unit's
/// key has no part of: set, they would give two builds of one key different code.
const CHANGES_CODE: &[&str] = &[
    "BLAZE_INLINE",
    "BLAZE_PROMOTE",
    "BLAZE_MIR_OPT",
    "BLAZE_REGALLOC",
    "BLAZE_LEAN",
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_EMIT",
    "BLAZE_NO_HOME",
    "BLAZE_BUILT_IN",
    // asm.rs: `0` lowers inline asm another way.
    "BLAZE_INLINE_ASM",
    // llvm.rs: LLVM's inliner threshold, read once per process; code inlined at another is other code.
    "BLAZE_LLVM_INLINE_THRESHOLD",
];

/// Whether the process's code generator is the one `spec` asks for: its IR as
/// [`check_built`] says, compiled to code that is position-independent or static as the
/// toolchain says, counting only where the code can count.
fn check_spec(unit: &Unit, spec: &Spec) -> Result<(), CodegenError> {
    check_built(unit, spec)?;
    let fail = |what: String| failed(unit, what);
    let (cranelift, stencil) = (spec.backend.is_cranelift(), spec.backend == Backend::Stencil);
    if spec.counted && !cranelift && !stencil {
        // alloc: an error
        return Err(fail(format!(
            "a unit for {:?} that counts its calls: only stencil and Cranelift code counts",
            spec.backend
        )));
    }
    let isa = isa(unit, unit_target(unit, spec)?)?;
    let pic = isa.flags().is_pic();
    if pic != (spec.toolchain.relocation == Relocation::Pic) {
        // alloc: an error
        return Err(fail(format!(
            "the unit's code is {:?}, the process's Cranelift {}",
            spec.toolchain.relocation,
            if pic { "position-independent" } else { "static" }
        )));
    }
    Ok(())
}

/// Whether the process's IR builder builds the IR `spec` asks for: the IR builder and the
/// back ends take the target and the frame pointers from the unit's toolchain (the latter
/// where the code is made, [`super::Function::frame_pointers`]), and the unit's target
/// features are a set the process compiles with (one ISA and one LLVM target machine a
/// target and set).
/// What the IR is compiled to (its relocation model, whether it counts) is no part of it.
pub(super) fn check_built(unit: &Unit, spec: &Spec) -> Result<(), CodegenError> {
    let fail = |what: String| failed(unit, what);
    if unit.backend != spec.backend {
        // alloc: the error's message
        return Err(fail(format!(
            "a unit for {:?} with a spec for {:?}",
            unit.backend, spec.backend
        )));
    }
    let toolchain = &spec.toolchain;
    if unit.arch != toolchain.arch {
        // alloc: the error's message
        return Err(fail(format!(
            "a unit for {:?} with a spec for {:?}",
            unit.arch, toolchain.arch
        )));
    }
    // wasm32 code is written from the IR by `wasm::lower` where Cranelift's would be made
    // (`emit_with`): no other back end has code of the form a page links
    // (`blaze_codegen::wasm::Code`), and none counts into cells, which a page has none of.
    // Nor is there a fuel runtime there (`blaze_stencil::fuel`): the plan meters native code
    // only.
    if toolchain.arch.is_wasm() && spec.fuel {
        // alloc: the error's message
        return Err(fail(format!(
            "a wasm32 unit for {:?} metered for fuel: only native code calls the fuel runtime",
            spec.backend
        )));
    }
    if toolchain.arch.is_wasm() && (!spec.backend.is_cranelift() || spec.counted) {
        // alloc: the error's message
        return Err(fail(format!(
            "a wasm32 unit for {:?}{}: wasm32 code is lowered from the IR as Cranelift's is \
             compiled (`wasm::lower`), and counts no calls",
            spec.backend,
            if spec.counted { " that counts its calls" } else { "" }
        )));
    }
    let target = unit_target(unit, spec)?;
    let arch = match target.arch {
        crate::Arch::X86_64 => Arch::X86_64,
        crate::Arch::Aarch64 => Arch::Aarch64,
        crate::Arch::Wasm32 => Arch::Wasm32,
        crate::Arch::Wasm64 => Arch::Wasm64,
    };
    if arch != toolchain.arch {
        // alloc: the error's message
        return Err(fail(format!(
            "the unit's toolchain names {} for {:?}, whose architecture is {arch:?}",
            toolchain.target, toolchain.arch
        )));
    }
    let cranelift = spec.backend.is_cranelift();
    let stencil = spec.backend == Backend::Stencil;
    // The CPU is the unit's set's ([`crate::Features`], [`crate::Target::cpu_of`]): LLVM is
    // told its name and features, Cranelift (which knows no CPU by name) its features, and a
    // stencil unit runs on it when it has every feature the stencils' CPU has
    // (`check_stencils`).
    if stencil {
        check_stencils(unit, target, spec)?;
    }
    // Cranelift's ISA and LLVM's target machine are made for the set the first time it is
    // compiled with; a set that can't be made is the unit's error, never the baseline's code.
    let features = target.features_of(&toolchain.cpu, &toolchain.features).map_err(fail)?;
    if cranelift {
        crate::target_with(target, features).map_err(|e| {
            // alloc: the error's message
            failed(
                unit,
                format!(
                    "Cranelift has no ISA for the target features {:?}: {e}",
                    toolchain.features
                ),
            )
        })?;
    }
    if (cranelift || stencil) && !spec.sanitize.is_empty() {
        return Err(fail(
            "-Zsanitizer and SanitizerCoverage are instrumented by the LLVM back end \
             (Backend::Llvm), which this unit is not compiled by"
                .to_owned(), // alloc: the error's message
        ));
    }
    if !matches!(spec.backend, Backend::Llvm { .. }) && spec.pgo != Pgo::None {
        // alloc: an error
        return Err(fail(format!(
            "PGO ({:?}: -Cprofile-generate, -Cprofile-use) is instrumented and used by the LLVM \
             back end (Backend::Llvm), and this unit is compiled by {:?}",
            spec.pgo, spec.backend
        )));
    }
    if let Some(knob) = set_knob() {
        // alloc: the error's message
        return Err(fail(format!(
            "`{knob}` is set, which changes code a unit's key has no part of"
        )));
    }
    Ok(())
}

/// What a stencil unit's spec must say of the stencils: the library the process copies from
/// is the one its key names (`Toolchain::stencils`), and its CPU and target features turn none
/// of the baseline's off. The stencils are the code of the target's baseline CPU
/// ([`crate::Target::cpu`]) and use no instruction past its features, so they run on any CPU
/// that has them all: a unit for `-C target-cpu=x86-64-v3` or `apple-m4` is such a CPU, whose
/// toolchain lists only `+` features. What the program executes is the same either way: a
/// `cfg(target_feature)` is the flags' (the front end's), and an intrinsic the stencils have
/// no code of is the unit's typed error, never another instruction. A CPU without one of the
/// baseline's features (`apple-a7` has no LSE, `generic` on macOS) lists it as `-`, and its
/// stencil unit is refused, naming it: its code is LLVM's (`Backend::Llvm`).
fn check_stencils(unit: &Unit, target: &crate::Target, spec: &Spec) -> Result<(), CodegenError> {
    let fail = |what: String| failed(unit, what);
    let toolchain = &spec.toolchain;
    let Some(stencils) = target.stencil() else {
        // alloc: an error
        return Err(fail(format!(
            "a stencil unit for {}, which has no stencils",
            toolchain.target
        )));
    };
    let id = blaze_stencil::library_id(&stencils).map_err(|e| failed(unit, e))?;
    match &toolchain.stencils {
        None => {
            // alloc: an error
            return Err(fail(format!(
                "a stencil unit for {}, whose toolchain names no stencil library",
                toolchain.target
            )));
        }
        Some(named) if !blaze_format::hex::is_encoding(named, &id) => {
            // alloc: an error
            return Err(fail(format!(
                "a stencil unit for the stencil library {named}, and the process copies from {}",
                blaze_format::hex::encode(&id)
            )));
        }
        Some(_) => {}
    }
    if let Some(off) = toolchain.features.iter().find(|it| it.starts_with('-')) {
        // alloc: an error
        return Err(fail(format!(
            "a stencil unit for CPU {} with the target features {:?}: stencils are the code of \
             {}'s baseline CPU {}, and `{off}` turns one of its features off; a unit for this CPU \
             is compiled by LLVM",
            toolchain.cpu,
            toolchain.features,
            toolchain.target,
            target.cpu()
        )));
    }
    Ok(())
}

/// The first of [`CHANGES_CODE`] that is set: a handful of lookups a unit, next to its compile.
fn set_knob() -> Option<&'static str> {
    CHANGES_CODE.iter().copied().find(|it| std::env::var_os(it).is_some())
}

/// The target `spec`'s toolchain names: the unit's own, which its code is compiled for. Units
/// of two targets compile side by side in one process.
pub(super) fn unit_target(
    unit: &Unit,
    spec: &Spec,
) -> Result<&'static crate::Target, CodegenError> {
    crate::Target::of(&spec.toolchain.target).map_err(|e| failed(unit, e))
}

// dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per target
fn isa(
    unit: &Unit,
    target: &'static crate::Target,
) -> Result<&'static dyn TargetIsa, CodegenError> {
    crate::isa_for(target).map(|it| &**it).map_err(|e| failed(unit, e))
}

impl Backends<'_> {
    /// `instance` and whether its references were listed for a unit that MIR-inlines
    /// ([`HirFrontend::inlines`]), as describing it listed them.
    pub(super) fn prepared(
        &self,
        db: &Db,
        hashes: &HashSource,
        unit: &Unit,
        spec: &Spec,
        (instance, walking): (&HirInstance, super::refs::Walking),
        refs: &Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        // What preparing one instance makes and lets go of (the placer's index, the scope
        // tables, the shape's tables) is in a frame of the worker's, reset when it returns;
        // what it keeps (the IR, its places, spans, variables and scopes) is the heap's.
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            // A panic building one function (an assertion of Cranelift's on the IR, one of
            // ours) fails that function with an error naming it and its crate, and through it
            // the unit, never the worker (AGENTS.md rule 20). Only a panic with a message is a
            // failure: a typed payload (a cancel, name resolution's typed stops) is control
            // flow its catcher reads further up, and keeps unwinding.
            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                built_function(db, hashes, unit, spec, (instance, walking), refs, frame)
            }));
            let function = match built {
                Ok(built) => built?,
                Err(payload) => match stdx::panic_text(&*payload) {
                    // The code generator's fault, not the front end's: no front item, so no
                    // reader looks for the body's diagnostics.
                    Some(message) => {
                        let named = super::Named(db, instance);
                        // alloc: an error
                        return Err(failed(
                            unit,
                            format!("{named}: blaze panicked building its code: {message}"),
                        ));
                    }
                    None => std::panic::resume_unwind(payload),
                },
            };
            let shape = shape::of(unit, &function, frame)
                // alloc: an error
                .map_err(|what| failed(unit, format!("{}: {what}", super::Named(db, instance))))?;
            let prepared = match unit.backend {
                // alloc: the prepared function, kept by the driver until its emit
                Backend::Cranelift { .. } => Prepared::Cranelift(Box::new(function)),
                // alloc: the prepared function, kept by the driver until its emit
                Backend::Llvm { .. } => Prepared::Llvm(Box::new(function)),
                // alloc: the prepared function, kept by the driver until its emit
                Backend::Stencil => Prepared::Stencil(Box::new(function)),
            };
            Ok((shape, prepared))
        })
    }
}

/// A request the walk did not list, `what`, with the settings the unit built its IR under and
/// the walks its references came from, as the walk takes them ([`refs::Walking::builds`]): a
/// unit whose settings no walk has is where the two disagree.
fn unwalked(what: &str, build: Build, walking: refs::Walking) -> String {
    let walks = walking.builds();
    // alloc: an error
    format!("{what}; the unit is built as {build:?}, its references walked as {:?}", &walks[..])
}

/// `instance`'s IR as `spec` builds it, against `refs`, with what each of its requests is in
/// the build: what a unit's [`Prepared`] holds, and what [`super::lowered()`] hands out. Its
/// tables are in `frame`; what it keeps is the heap's.
#[allow(clippy::too_many_arguments)]
pub(super) fn built_function(
    db: &Db,
    hashes: &HashSource,
    unit: &Unit,
    spec: &Spec,
    (instance, walking): (&HirInstance, super::refs::Walking),
    refs: &Refs,
    frame: &Frame,
) -> Result<Function, CodegenError> {
    // alloc: an error
    let fail = |what: String| failed(unit, format!("{}: {what}", super::Named(db, instance)));
    let inlined = walking.inlined;
    // Its body is built against the program's dyn facts where its references were walked so
    // (`super::devirt::walking`) and the unit inlines: only then are its direct callees among
    // them. A unit that inlines nothing (a generic instance in a crate built at a lower level
    // than another that asks for it, as a proc macro's wasm module is) builds the plain body,
    // which every walk lists too (`refs::code_refs` walks `Build::WALK` first).
    let plain = Build::of(spec);
    let build = Build { world: walking.world.filter(|_| plain.inlines), ..plain };
    if build.inlines && !inlined {
        // alloc: an error
        return Err(fail(format!(
            "a unit whose MIR is inlined ({:?}) of references listed for units that \
             inline none: what the inlined code calls is not among them",
            unit.backend
        )));
    }
    let walked = refs::listed(db, instance, walking).map_err(fail)?;
    if walked.len() != refs.len() {
        // alloc: an error
        return Err(fail(format!(
            "{} references given for an instance of {}",
            refs.len(),
            walked.len()
        )));
    }
    let mut placer = Placer::listed_in(walked, frame);
    let target = unit_target(unit, spec)?;
    let krate = compiled_in(db, instance);
    // The IR builder lays the code out for its crate's target, the back end for the unit's.
    if let Some(krate) = krate {
        let built_for = crate::target_of_crate(db, krate).map_err(|e| failed(unit, e))?;
        if !std::ptr::eq(built_for, target) {
            // alloc: the error's message
            return Err(fail(format!(
                "the unit is for {}, its crate's target is {}",
                target.triple(),
                built_for.triple()
            )));
        }
    }
    // Its body built from its MIR: a failure is the front end's, which the error carries.
    let mut code = built(db, instance, target, spec.overflow_checks, build)
        .map_err(|why| super::failed_in_front(db, Some(unit), None, instance, why))?;
    let (places, source, variables) = match &mut code {
        Built::Ir(pending) => {
            let requests = &mut pending.wants.requests;
            let places = place_requests(db, hashes, krate, &mut placer, requests)
                .map_err(|what| fail(unwalked(&what, build, walking)))?;
            (places, pending.source.take(), pending.variables.take())
        }
        Built::Assembled(_, requests) => {
            let places = place_requests(db, hashes, krate, &mut placer, requests)
                .map_err(|what| fail(unwalked(&what, build, walking)))?;
            (places, None, None)
        }
    };
    let lines = spec.debuginfo != DebugInfo::None;
    let spans = match (&source, lines) {
        (Some(source), true) => written_spans(db, hashes, source).map_err(fail)?,
        // alloc: none, empty
        _ => Vec::new(),
    };
    let told = spec.debuginfo.tells_locals();
    let (variables, mut scopes) = match (&source, variables) {
        (Some(source), Some((env, variables))) if told => {
            let krate = env.as_ref(db).krate;
            named(db, hashes, krate, source.owner, &variables, &source.spans, frame)
                .map_err(fail)?
        }
        _ => Default::default(),
    };
    if let (Some(source), true) = (&source, lines) {
        inlined_scopes(db, hashes, source, &mut scopes).map_err(fail)?;
    }
    // A test's fuel (#1605): ticks at the entry and loop heads, in the IR every back end
    // compiles, before the shape is taken of it. The fuel runtime's own functions tick none.
    let metered =
        spec.fuel && matches!(code, Built::Ir(_)) && !fuel_runtime(db, instance).map_err(fail)?;
    if let (Built::Ir(pending), true) = (&mut code, metered) {
        let call_conv = isa(unit, target)?.default_call_conv();
        blaze_stencil::fuel::meter(
            &mut pending.func,
            &blaze_stencil::fuel::Runtime::blaze(),
            call_conv,
        )
        .map_err(fail)?;
    }
    if let Built::Ir(pending) = &mut code {
        if !lines {
            pending.func.stencil.srclocs.clear();
        }
        if matches!(unit.backend, Backend::Cranelift { .. } | Backend::Stencil) {
            colocate(&mut pending.func, &places, placer.refs(), frame);
        }
    }
    let built_in = match &code {
        Built::Ir(pending) => pending.built_in(),
        Built::Assembled(..) => &[],
    };
    let inlined = super::frontend::inlined(db, hashes, instance, built_in).map_err(fail)?;
    let (features, own_features) = features(db, spec, target, instance).map_err(fail)?;
    let wasm = if unit.arch.is_wasm() {
        let requests = match &code {
            Built::Ir(pending) => &pending.wants.requests,
            Built::Assembled(_, requests) => requests,
        };
        super::wasm::header(db, target, instance, requests).map_err(fail)?
    } else {
        Default::default()
    };
    Ok(Function {
        code,
        places,
        spans,
        variables,
        scopes,
        inlined,
        hint: hint(db, instance),
        unsanitized: unsanitized(db, spec, instance).map_err(fail)?,
        features,
        target,
        frame_pointers: spec.toolchain.frame_pointers,
        release: Default::default(),
        // A fold would share one function's counter cells between its members, and
        // give one function's code of its own target features to the others; a metered
        // body's ticks are its own too.
        foldable: !spec.counted
            && !metered
            && !own_features
            && foldable(db, instance).map_err(fail)?,
        wasm,
    })
}

/// The crate an instance's code is compiled in; `None` for assembly, which is no crate's.
pub(super) fn compiled_in(db: &Db, instance: &HirInstance) -> Option<Crate> {
    match &instance.0 {
        Kind::Code(_, env) => Some(env.as_ref(db).krate),
        Kind::Start(main) => Some(main.krate(db)),
        Kind::Allocator(forward) => Some(forward.krate),
        Kind::Asm(..) => None,
    }
}

/// What a function's `#[inline]` asks of LLVM, as `imports::build` reads it.
fn hint(db: &Db, instance: &HirInstance) -> Hint {
    let Kind::Code(Instance { def: InstanceDef::Fn(func), .. }, _) = &instance.0 else {
        return Hint::None;
    };
    let flags = AttrFlags::query(db, (*func).into());
    if flags.contains(AttrFlags::INLINE_NEVER) {
        Hint::Never
    } else if flags.contains(AttrFlags::INLINE_ALWAYS) {
        Hint::Always
    } else if flags.contains(AttrFlags::IS_INLINE) {
        Hint::Inline
    } else {
        Hint::None
    }
}

/// The target features the instance's code is compiled with, and whether they are more than
/// its unit's: the unit's (`Toolchain::features`) and what the `#[target_feature(enable)]` of
/// its function turns on with what that implies (`TargetFeatures::from_fn`), as rustc gives
/// each function a `target-features` of its own. As rustc's `codegen_instance_attrs`, a vtable
/// or reify shim has those of the function it is of, and drop glue and the `FnOnce` shims
/// none; as its `codegen_fn_attrs`, a closure or coroutine has its function's. They change the
/// code only, never `cfg(target_feature)`.
fn features(
    db: &Db,
    spec: &Spec,
    target: &crate::Target,
    instance: &HirInstance,
) -> Result<(crate::Features, bool), String> {
    let unit = target.features_of(&spec.toolchain.cpu, &spec.toolchain.features)?;
    let func = match &instance.0 {
        Kind::Code(
            Instance { def: InstanceDef::Fn(func) | InstanceDef::VtableShim(func), .. },
            _,
        ) => Some(*func),
        Kind::Code(
            Instance {
                def:
                    InstanceDef::Closure(_)
                    | InstanceDef::Coroutine(_)
                    | InstanceDef::CoroutineByMove(_)
                    | InstanceDef::ReifyShim { .. },
                ..
            },
            _,
        ) => match super::frontend::item_of(db, &instance.0)? {
            ItemId::Function(func) => Some(func),
            _ => None,
        },
        Kind::Code(..) | Kind::Start(_) | Kind::Allocator(_) | Kind::Asm(..) => None,
    };
    let Some(func) = func else { return Ok((unit, false)) };
    multiversioned(db, spec, target, func)?;
    let enabled = hir_ty::TargetFeatures::from_fn(db, func).known();
    if enabled.is_empty() {
        return Ok((unit, false));
    }
    let set = with_enabled(target, &spec.toolchain.cpu, &spec.toolchain.features, enabled.names())?;
    Ok((set, set != unit))
}

/// `#[blaze::multiversion]` on `func` (docs/v2/plan/design/simd.md): its levels are checked
/// against the unit's features, and a plan with no clone (the unit already has every level,
/// or the target has none) is the function as it is. The clones themselves, their dispatch
/// cell and the code that fills it are not built yet, so a plan that has one stops here,
/// naming the function and the levels: compiling only the baseline would hand back slower
/// code than was asked for and say nothing (`no-fallback`).
///
/// SAFETY of what replaces this refusal, stated where the attribute becomes target features:
/// a clone is this function compiled as if `#[target_feature(enable = ..)]` named its level's
/// added features, which rustc makes unsafe to call because the caller must know the CPU has
/// them. The attribute is safe because a clone has no name and no address a program can take,
/// and is entered only (1) through the function's dispatch cell, which holds the baseline
/// until it is filled with a clone `Plan::select` picked from the features std's detection
/// reported, so every feature the clone adds is present, or (2) directly from a clone whose
/// set contains this one's, which by induction already runs on such a CPU. The unit's own
/// features are the program's precondition, as for every function of the unit.
fn multiversioned(
    db: &Db,
    spec: &Spec,
    target: &crate::Target,
    func: hir_def::FunctionId,
) -> Result<(), String> {
    if !AttrFlags::query(db, func.into()).contains(AttrFlags::BLAZE_MULTIVERSION) {
        return Ok(());
    }
    let name = FunctionSignature::of(db, func).name.symbol().as_str();
    let unit = unit_features(target, &spec.toolchain.features);
    let plan = match hir_ty::multiversion_plan(db, func, &unit) {
        None => return Ok(()),
        // alloc: an error
        Some(Err(refusal)) => return Err(format!("function `{name}`: {refusal}")),
        Some(Ok(plan)) => plan,
    };
    if plan.clones().is_empty() {
        return Ok(());
    }
    // alloc: an error
    let levels: Vec<&str> = plan.clones().iter().map(|it| &*it.name).collect();
    Err(format!(
        "function `{name}` is `#[blaze::multiversion]` and this unit would compile a clone of it \
         for each of the levels {levels:?}: blaze does not compile a function's clones yet \
         (docs/v2/plan/design/simd.md, \"Built in slice 1\"); remove the attribute, or build \
         the unit with those target features"
    ))
}

/// A unit's target features as rustc's tables name them: the baseline's of its target with
/// what `toolchain` (its sorted `+name` and `-name`) turns on and off. A name no table of
/// rustc's has is left out, and one of another arch's table (`fp-armv8` of AArch64's baseline,
/// which rustc folds into `neon`) implies nothing there: a level never names either, since
/// `#[blaze::multiversion]`'s levels are checked against the target's own table.
fn unit_features(target: &crate::Target, toolchain: &[String]) -> hir_ty::TargetFeatureSet {
    let off = |name: &str| toolchain.iter().any(|it| it.strip_prefix('-') == Some(name));
    let baseline = super::toolchain::baseline(target).iter().copied().filter(|it| !off(it));
    let on = toolchain.iter().filter_map(|it| it.strip_prefix('+'));
    baseline.chain(on).filter_map(hir_ty::TargetFeature::from_name).collect()
}

/// The set of `toolchain` (a unit's sorted features, for its CPU `cpu`) with `enabled`
/// (rustc's names, each with what it implies) turned on: a feature the toolchain turns off is
/// on again, and one the baseline has or the toolchain turns on already is not named twice.
fn with_enabled<'a>(
    target: &crate::Target,
    cpu: &str,
    toolchain: &[String],
    enabled: impl Iterator<Item = &'a str>,
) -> Result<crate::Features, String> {
    let baseline = super::toolchain::baseline(target);
    // alloc: per function of a `#[target_feature]`, its set's names
    let mut names: Vec<String> = toolchain.to_vec();
    for name in enabled {
        let off = names.iter().position(|it| it.strip_prefix('-') == Some(name));
        if let Some(off) = off {
            names.remove(off);
        } else if !baseline.iter().any(|it| *it == name)
            && !names.iter().any(|it| it.strip_prefix('+') == Some(name))
        {
            // alloc: per function of a `#[target_feature]`, a feature's name
            names.push(format!("+{name}"));
        }
    }
    names.sort_unstable();
    target.features_of(cpu, &names)
}

/// Which of `spec`'s sanitizers are off for the instance. As rustc's `codegen_instance_attrs`,
/// a shim has the `#[sanitize]` of the item it is of ([`super::frontend::item_of`]), a
/// closure its function's; the C `main` and the allocator shim are no item and have none
/// off. SanitizerCoverage is off for a `__sanitizer_*` symbol, which its pass skips by name
/// in rustc's build and which here it would otherwise trace.
fn unsanitized(db: &Db, spec: &Spec, instance: &HirInstance) -> Result<Sanitizers, String> {
    let sanitize = &spec.sanitize;
    if sanitize.is_empty() {
        return Ok(Sanitizers::NONE);
    }
    let off = match &instance.0 {
        Kind::Code(..) => match super::frontend::item_of(db, &instance.0)? {
            ItemId::Function(func) => hir_def::attrs::disabled_sanitizers(db, func),
            other => {
                // alloc: the error's message
                return Err(format!(
                    "the `#[sanitize]` of {other:?}, which is no function: db's \
                     `disabled_sanitizers` takes a function"
                ));
            }
        },
        Kind::Start(_) | Kind::Allocator(_) | Kind::Asm(..) => 0,
    };
    let mut unsanitized = Sanitizers(off & sanitize.sanitizers.0);
    let callback = || {
        super::frontend::symbol_of(db, &instance.0)
            .map(|it| it.is_some_and(|it| it.starts_with("__sanitizer_")))
    };
    if sanitize.coverage.is_on() && callback()? {
        unsanitized = unsanitized.union(Sanitizers::COVERAGE);
    }
    Ok(unsanitized)
}

/// Whether a fold may give the instance another's code: its address is no symbol's, and it
/// takes no C variadic arguments, whose `va_list` a thunk could not pass on.
fn foldable(db: &Db, instance: &HirInstance) -> Result<bool, String> {
    if super::frontend::symbol_of(db, &instance.0)?.is_some() {
        return Ok(false);
    }
    Ok(match &instance.0 {
        Kind::Code(Instance { def: InstanceDef::Fn(func), .. }, _) => {
            !FunctionSignature::of(db, *func).is_varargs()
        }
        Kind::Code(..) => true,
        Kind::Asm(..) | Kind::Start(_) | Kind::Allocator(_) => false,
    })
}

/// The IR of an instance, built as a unit with overflow checks `checked` asks, under the
/// unit's settings `build`. They come in as an argument, never off the thread: the compile
/// waits in queries, after which its fiber may be on another thread, or interleaved with
/// another unit's compile on this one (rule 27). Each body it reads is keyed by the options
/// `build` derives for it ([`Build::options`]), so what it builds follows from its arguments
/// alone (rule 21). An `asm!` block's code is its text, the same under every unit's settings.
pub(super) fn built(
    db: &Db,
    instance: &HirInstance,
    target: &'static crate::Target,
    checked: bool,
    build: Build,
) -> Result<Built, String> {
    // A unit's compile builds in no compile's hot functions (`&mut None`, `tier::Inlining`):
    // what it builds in is the unit's (`Build::built_in`).
    let unit = Some(build);
    let produced = match &instance.0 {
        Kind::Code(instance, env) => {
            let instance = Instance { checked, ..*instance };
            instance_code(db, instance, *env, Detach::Unit, unit, &mut None)
        }
        Kind::Start(main) => {
            let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main.krate(db) };
            compile_as(db, env, None, false, checked, true, unit, &mut None, |fx| {
                fx.start_shim(*main)
            })
        }
        Kind::Asm(text, _) => return assembled(target, text),
        Kind::Allocator(forward) => super::allocator::built(db, forward, checked, unit),
    }
    .map_err(|e| e.to_string())?; // alloc: the error's message
    Ok(match produced {
        Produced::Pending(pending) => Built::Ir(pending),
        Produced::Code(compiled) => Built::Assembled(compiled.code, compiled.requests),
    })
}

/// The function of an `asm!` block: its assembly, which names nothing.
fn assembled(target: &'static crate::Target, text: &str) -> Result<Built, String> {
    if target.is_wasm() {
        // alloc: the error's message
        return Err("an `asm!` or `global_asm!` function on wasm32: blaze assembles x86-64 and \
                    AArch64, and wasm32's ISA is one whose settings only the IR borrows"
            .to_owned());
    }
    let isa = crate::isa_for(target).map_err(|e| e.to_string())?; // alloc: the error's message
    // alloc: only an `asm!` function, whose assembler takes the triple as text
    let assembled = crate::asm::assemble(&isa.triple().to_string(), text)
        // alloc: the error's message, which names the block (its instance has no other name)
        .map_err(|e| format!("{e}; the block: `{}`", crate::asm::summary(text)))?;
    if !assembled.relocs.is_empty() {
        // alloc: the error's message
        return Err("an `asm!` function that names a symbol".to_owned());
    }
    let code = fragment::Code {
        signature: ir::Signature::new(isa.default_call_conv()),
        // clone: only an `asm!` function; the assembler's cache keeps its copy, `Code` owns this
        bytes: assembled.code.clone(),
        align: 16,
        relocs: fragment::Relocs::default(),
        unwind: None,
        lines: Vec::new(), // alloc: empty, allocates nothing
        slots: SlotPlaces::default(),
    };
    Ok(Built::Assembled(code, Requests::default()))
}

/// Where each statement the function's line rows are of is written, inlined code's in the
/// body it was inlined from.
fn written_spans(
    db: &Db,
    hashes: &HashSource,
    source: &fragment::Source,
) -> Result<Vec<SpanRef>, String> {
    let mut spans = Vec::with_capacity(source.spans.len());
    for at in 0..source.spans.len() as u32 {
        let Some(written) = source.written(at, None) else {
            // alloc: the error's message
            return Err(format!(
                "statement {at} of the function, which has no place in the source"
            ));
        };
        spans.push(hashes.span(db, written.owner, written.span)?);
    }
    Ok(spans)
}

/// Tells `scopes` of the bodies the MIR optimizer inlined into the function: each one's call
/// site, and which of them each statement is of.
fn inlined_scopes(
    db: &Db,
    hashes: &HashSource,
    source: &fragment::Source,
    scopes: &mut side::Scopes,
) -> Result<(), String> {
    if source.inlined.is_empty() {
        return Ok(());
    }
    scopes.inlined.reserve(source.scopes.len());
    for (index, scope) in source.scopes.iter().enumerate() {
        let Some(at) = source.call_site(index as u32, None) else {
            // alloc: the error's message
            return Err(format!(
                "the call inlined scope {index} is inlined at, of {:?} within inlined callee \
                 {:?}, which is no item's body",
                scope.call_site, scope.parent
            ));
        };
        let call_site = hashes.span(db, at.owner, at.span)?;
        scopes.inlined.push(side::InlinedScope { parent: scope.parent, call_site });
    }
    scopes.inlined_of_span =
        // alloc: the function's table, kept with it, exact size; only one with inlined bodies
        (0..source.spans.len() as u32).map(|at| source.scope_of(at)).collect();
    Ok(())
}

/// The named locals a debugger is told of, and the scopes of the body they are bound in: a
/// name a macro of the compiler's made up is no name of the source. `spans` are the
/// statements the function's line rows are of. Their types are of a body monomorphized in
/// `krate`.
fn named(
    db: &Db,
    hashes: &HashSource,
    krate: Crate,
    owner: DefWithBodyId,
    variables: &[fragment::Variable],
    spans: &[MirSpan],
    frame: &Frame,
) -> Result<(Vec<side::Variable>, side::Scopes), String> {
    let body = Body::of(db, owner);
    let mut told: Vec<&fragment::Variable, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    // Only the names lowering makes start with `<`, never one written, so which locals are
    // told follows from the code and the side table needs no name in its key.
    told.extend(variables.iter().filter(|it| !body[it.binding].name.as_str().starts_with('<')));
    let (scopes, of_binding) = scoped(db, owner, &told, spans, frame);
    // alloc: the named locals, kept in the prepared function
    let mut named = Vec::with_capacity(told.len());
    for variable in told.iter().copied() {
        named.push(side::Variable {
            binding: variable.binding.into_raw().into_u32(),
            param: variable.param,
            ty: hashes.ty(db, evaluated(db, Some(krate), variable.ty.as_ref())?)?,
            slot: variable.slot,
            pointer: variable.pointer,
            scope: of_binding.get(&variable.binding).copied(),
        });
    }
    Ok((named, scopes))
}

/// The scopes of the body of `owner` that `variables` are bound in, outer ones first, which
/// of them each binding is of, and which each of `spans` is of. A binding of the outermost
/// scope, which a parameter is, is known in all of the function and is of none. The code that
/// tries the pattern of an arm is of the scope its `match` is in; other code of no expression
/// is of the scope of the code before it.
fn scoped<'f>(
    db: &Db,
    owner: DefWithBodyId,
    variables: &[&fragment::Variable],
    spans: &[MirSpan],
    frame: &'f Frame,
) -> (side::Scopes, FrameHashMap<'f, BindingId, u32>) {
    let scopes = ExprScopes::of(db, owner);
    let body = Body::of(db, owner);
    let outermost = scopes.scope_chain(scopes.scope_for(body.root_expr())).last();
    // The scope of each binding told of, and of no other: the walk stops once all are found.
    // By their raw index, which is what the sort keyed them by before: the radix sort's
    // buffer is the thread's.
    let mut wanted: Vec<u32, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    wanted.extend(variables.iter().map(|it| it.binding.into_raw().into_u32()));
    blaze_mem::scratch::with_u32s(|scratch| {
        blaze_ids::radix::sort_copy_by_key_in(&mut wanted, |&it| it, scratch);
    });
    wanted.dedup();
    let mut bound = frame_map::hash_map_with_capacity(wanted.len(), frame);
    let mut seen = frame_map::hash_set(frame);
    'scopes: for (_, &scope) in scopes.scope_by_expr().iter() {
        for scope in scopes.scope_chain(Some(scope)).take_while(|&it| seen.insert(it)) {
            for entry in scopes.entries(scope) {
                if wanted.binary_search(&entry.binding().into_raw().into_u32()).is_ok() {
                    bound.insert(entry.binding(), scope);
                    if bound.len() == wanted.len() {
                        break 'scopes;
                    }
                }
            }
        }
    }
    let mut told: FrameHashMap<'_, ScopeId, u32> = frame_map::hash_map(frame);
    // alloc: the scopes' parents, kept in the prepared function
    let mut parents = Vec::new();
    let mut of_binding = frame_map::hash_map(frame);
    // A binding's scopes not told of yet, outermost first: one list for the unit, refilled
    // for each binding.
    let mut chain: Vec<ScopeId, &Frame> = Vec::new_in(frame);
    for variable in variables {
        let Some(&scope) = bound.get(&variable.binding).filter(|&&it| Some(it) != outermost) else {
            continue;
        };
        chain.clear();
        chain.extend(
            scopes
                .scope_chain(Some(scope))
                .take_while(|it| Some(*it) != outermost && !told.contains_key(it))
                .filter(|&it| it == scope || !scopes.entries(it).is_empty()),
        );
        chain.reverse();
        for &scope in chain.iter() {
            let parent =
                scopes.scope_chain(Some(scope)).skip(1).find_map(|it| told.get(&it).copied());
            told.insert(scope, parents.len() as u32);
            parents.push(parent);
        }
        of_binding.insert(variable.binding, told[&scope]);
    }
    if told.is_empty() {
        return (side::Scopes::default(), of_binding);
    }
    let mut tried = frame_map::hash_map(frame);
    if spans.iter().any(|it| matches!(it, MirSpan::PatId(_))) {
        for (expr, it) in body.exprs() {
            if let hir_def::hir::Expr::Match { arms, .. } = it {
                for arm in arms.iter() {
                    body.walk_pats(arm.pat, &mut |pat| drop(tried.insert(pat, expr)));
                }
            }
        }
    }
    let innermost =
        |scope: Option<ScopeId>| match scopes.scope_chain(scope).find_map(|it| told.get(&it)) {
            Some(&index) => side::SpanScope::In(index),
            None => side::SpanScope::Outside,
        };
    let mut of_span = Vec::with_capacity(spans.len());
    of_span.extend(spans.iter().map(|span| match *span {
        MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => innermost(scopes.scope_for(expr)),
        MirSpan::PatId(pat) => match tried.get(&pat) {
            Some(&of) => innermost(scopes.scope_for(of)),
            None => side::SpanScope::Before,
        },
        _ => side::SpanScope::Before,
    }));
    (side::Scopes { parents, of_span, ..Default::default() }, of_binding)
}

/// Makes every call and every address of blaze's own code and data direct: in v2 the linker
/// places them all, so nothing of them goes through the GOT. A symbol of outside code is
/// called directly too (through the PLT where it is in another object), but its address is
/// left as the IR builder takes it.
fn colocate(func: &mut ir::Function, places: &Places, refs: &[Ref], frame: &Frame) {
    let outside = |place: u32| {
        matches!(refs.get(place as usize), Some(FrontRef::Extern(_) | FrontRef::Weak(_)) | None)
    };
    let ir::Function { params, stencil, .. } = func;
    let names = params.user_named_funcs();
    let own = |name: &ir::ExternalName| -> Option<bool> {
        let ir::ExternalName::User(at) = name else { return None };
        let name = &names[*at];
        match name.namespace {
            0 => Some(!outside(*places.funcs.get(name.index as usize)?)),
            1 => Some(match places.datas.get(name.index as usize)? {
                DataPlace::Place(place) => !outside(*place),
                DataPlace::Own
                | DataPlace::Position(_)
                | DataPlace::Text(_)
                | DataPlace::Nowhere => true,
            }),
            _ => None,
        }
    };
    // One bit a callee, in the frame.
    let words = frame.alloc_slice_fill_with(stencil.dfg.ext_funcs.len().div_ceil(64), |_| 0);
    let mut addressed: Bits<&mut [u64]> = Bits::from_words(words);
    for block in stencil.layout.blocks() {
        for inst in stencil.layout.block_insts(block) {
            if let ir::InstructionData::FuncAddr { func_ref, .. } = stencil.dfg.insts[inst] {
                addressed.insert(func_ref.as_u32() as usize);
            }
        }
    }
    for (func_ref, ext) in stencil.dfg.ext_funcs.iter_mut() {
        let at = func_ref.as_u32() as usize;
        match own(&ext.name) {
            Some(true) => ext.colocated = true,
            Some(false) if !addressed.contains(at) => ext.colocated = true,
            _ => {}
        }
    }
    for (_, global) in stencil.global_values.iter_mut() {
        if let ir::GlobalValueData::Symbol { name, colocated, tls: false, .. } = global
            && own(name) == Some(true)
        {
            *colocated = true;
        }
    }
}

/// Compiles a prepared function with Cranelift, against `refs`; `counted`, its code counts its
/// calls and back edges into its own cells (`blaze_stencil::tier::instrument`).
pub(crate) fn emit_cranelift(
    unit: &Unit,
    function: Box<Function>,
    refs: &Refs,
    counted: bool,
) -> Result<Emitted, CodegenError> {
    let (features, target, frame_pointers) =
        (function.features, function.target, function.frame_pointers);
    emit_built(unit, function, refs, |func, named| {
        compiled(unit, target, func, named, counted, features, frame_pointers)
    })
}

/// Compiles a prepared function by copy-and-patch (`blaze_stencil`), against `refs`.
/// Copies a prepared function's stencils, against `refs`; `counted`, its code counts its calls
/// and back edges into its own cells.
fn emit_stencil(
    unit: &Unit,
    function: Box<Function>,
    refs: &Refs,
    counted: bool,
) -> Result<Emitted, CodegenError> {
    let target = function.target;
    emit_built(unit, function, refs, |func, named| stenciled(unit, target, func, named, counted))
}

/// A prepared function's code, its IR made code by `backend` (given the stack slots of its
/// named locals), against `refs`. An assembly function's code is the assembler's, whatever
/// the backend.
fn emit_built(
    unit: &Unit,
    function: Box<Function>,
    refs: &Refs,
    backend: impl FnOnce(ir::Function, &[ir::StackSlot]) -> Result<fragment::Code, CodegenError>,
) -> Result<Emitted, CodegenError> {
    if function.places.refs as usize != refs.len() {
        return Err(failed(
            unit,
            // alloc: the error's message
            format!(
                "{} references given for a function prepared against {}",
                refs.len(),
                function.places.refs
            ),
        ));
    }
    let Function { code, places, spans, variables, scopes, inlined, wasm, target, .. } = *function;
    let named = Named { spans: &spans, variables: &variables, scopes: &scopes, inlined };
    match code {
        Built::Ir(pending) => {
            let code = backend(pending.func, &pending.named)?;
            told(unit, target, (&places, &wasm), &pending.wants.requests, code, refs, named)
        }
        Built::Assembled(code, requests) => {
            told(unit, target, (&places, &wasm), &requests, code, refs, named)
        }
    }
}

/// What a debugger is told of a function: [`Function::spans`], `variables` and `scopes`; and
/// what its code has built in, [`Function::inlined`].
struct Named<'a> {
    spans: &'a [SpanRef],
    variables: &'a [side::Variable],
    scopes: &'a side::Scopes,
    inlined: Vec<CodeHash>,
}

impl Function {
    fn named(&self) -> Named<'_> {
        Named {
            spans: &self.spans,
            variables: &self.variables,
            scopes: &self.scopes,
            // The compiled function owns its list, and `emitted` has the prepared one borrowed.
            // clone: empty, allocating nothing, for nearly every function
            inlined: self.inlined.clone(),
        }
    }
}

/// What `emit` gives of a function's code, Cranelift's or LLVM's: its relocations and data
/// through the function's places at `refs`, its lines and the side table of its named locals.
pub(crate) fn emitted(
    unit: &Unit,
    function: &Function,
    code: fragment::Code,
    refs: &Refs,
) -> Result<Emitted, CodegenError> {
    told(
        unit,
        function.target,
        (&function.places, &function.wasm),
        function.requests(),
        code,
        refs,
        function.named(),
    )
}

fn told(
    unit: &Unit,
    target: &'static crate::Target,
    (places, wasm): (&Places, &super::wasm::Header),
    requests: &Requests,
    code: fragment::Code,
    refs: &Refs,
    Named { spans, variables, scopes, inlined }: Named<'_>,
) -> Result<Emitted, CodegenError> {
    let fail = |what: String| failed(unit, what);
    if refs.len() < places.refs as usize {
        // alloc: the error's message
        return Err(fail(format!(
            "{} references given for a function prepared against {}",
            refs.len(),
            places.refs
        )));
    }
    let isa = isa(unit, target)?;
    let (compiled, side) = resolve(places, refs, |resolve| {
        let side = side::of(variables, scopes, &code, resolve.frame())?;
        let compiled = super::convert::function(
            resolve.frame(),
            unit.instance.key(),
            unit.arch,
            isa,
            code,
            requests,
            resolve,
            spans,
            side.as_ref().map(|it| it.1),
            wasm,
        )?;
        Ok::<_, String>((compiled, side))
    })
    .map_err(fail)?;
    let function = CompiledFunction { inlined, ..compiled };
    // alloc: empty but for a metered function
    let mut extra = Vec::new();
    fuel_externs(&function, &mut extra);
    Ok(Emitted { function, side: side.map(|it| it.0), extra })
}

/// Adds to `extra` the fuel runtime's functions `function` calls (`blaze_stencil::fuel`),
/// which no reference lists: the template keeps them as its own outside symbols.
pub(super) fn fuel_externs(function: &CompiledFunction, extra: &mut Vec<String>) {
    for target in function.relocs.targets() {
        if let blaze_codegen::Target::Extern(name) = target
            && name.starts_with(blaze_stencil::fuel::PREFIX)
            && !extra.contains(name)
        {
            // clone: one of the two names of a metered function
            extra.push(name.clone());
        }
    }
}

/// Whether `instance` is a function of the fuel runtime (its symbol starts with
/// `blaze_stencil::fuel::PREFIX`), which metered code calls and which must not tick itself.
fn fuel_runtime(db: &Db, instance: &HirInstance) -> Result<bool, String> {
    if !matches!(instance.0, Kind::Code(..)) {
        return Ok(false);
    }
    Ok(super::frontend::symbol_of(db, &instance.0)?
        .is_some_and(|it| it.starts_with(blaze_stencil::fuel::PREFIX)))
}

/// Compiles `ir` in place of a prepared function's IR, its names being the function's: a
/// fold's body, emitted as its first member, or one of its thunks, which names the body
/// last of `refs` and has none of its member's lines or named locals.
fn emit_ir(
    unit: &Unit,
    function: &Function,
    ir: ir::Function,
    refs: &Refs,
) -> Result<Emitted, CodegenError> {
    let own = function.places.refs as usize;
    let thunk = refs.len() == own + 1;
    if !thunk && refs.len() != own {
        return Err(failed(
            unit,
            // alloc: the error's message
            format!("{} references given for a fold of a function of {own}", refs.len()),
        ));
    }
    let Built::Ir(pending) = &function.code else {
        return Err(failed(unit, "a fold of an assembly function"));
    };
    // A fold's members never count: `merge` refuses them.
    if thunk {
        let code = compiled(
            unit,
            function.target,
            ir,
            &[],
            false,
            function.features,
            function.frame_pointers,
        )?;
        let none = side::Scopes::default();
        // alloc: empty, allocates nothing
        let named = Named { spans: &[], variables: &[], scopes: &none, inlined: Vec::new() };
        told(
            unit,
            function.target,
            (&function.places, &function.wasm),
            function.requests(),
            code,
            refs,
            named,
        )
    } else {
        let code = compiled(
            unit,
            function.target,
            ir,
            &pending.named,
            false,
            function.features,
            function.frame_pointers,
        )?;
        emitted(unit, function, code, refs)
    }
}

/// Cranelift's code of the IR at the unit's level, `named` being the stack slots of its
/// named locals; `counted`, counting into the function's own cells; with the ISA of the
/// function's target features ([`features`]), keeping every frame pointer when
/// `frame_pointers` ([`Function::frame_pointers`]).
fn compiled(
    unit: &Unit,
    target: &'static crate::Target,
    func: ir::Function,
    named: &[ir::StackSlot],
    counted: bool,
    features: crate::Features,
    frame_pointers: bool,
) -> Result<fragment::Code, CodegenError> {
    let Backend::Cranelift { speed } = unit.backend else {
        // alloc: the error's message
        return Err(failed(unit, format!("Cranelift's code of a unit for {:?}", unit.backend)));
    };
    let isa = isa(unit, target)?;
    crate::with_context(func, |context| {
        // Before the pairs are lowered: the counting is of the IR's own calls and loops.
        if counted {
            blaze_stencil::tier::instrument(&mut context.func).map_err(|e| failed(unit, e))?;
        }
        let pairs = crate::float_pair::lower(&mut context.func);
        let mut code = crate::emit_lent(context, target, speed, features, frame_pointers)
            .map_err(|e| failed(unit, e))?;
        if let Some(frame) = context.compiled_code().and_then(|it| it.buffer.frame_layout()) {
            code.slots = SlotPlaces::of_frame(isa, frame, &context.func, named.iter().copied());
        }
        if let Some(pairs) = pairs {
            pairs.hold(context, &mut code).map_err(|e| failed(unit, e))?;
        }
        Ok(code)
    })
}

/// Copy-and-patch code of the IR (`blaze_stencil::compile_to`), in the terms Cranelift's
/// compile gives ([`fragment::Code`]), `named` being the stack slots of its named locals.
/// What the stencils can't compile is the unit's error, never another backend's code.
/// A prepared function's IR as copy-and-patch compiles it, which [`super::lowered()`] hands out:
/// a pair of floats `windows_fastcall` can't return is returned as two integers, in its
/// signature and its calls alike (`float_pair::lower`), whose `hold` gives the code back the
/// signature its callers know.
pub(super) fn stencil_ir(func: &mut ir::Function) -> Option<crate::float_pair::Lowered> {
    crate::float_pair::lower(func)
}

fn stenciled(
    unit: &Unit,
    target: &'static crate::Target,
    func: ir::Function,
    named: &[ir::StackSlot],
    counted: bool,
) -> Result<fragment::Code, CodegenError> {
    if unit.backend != Backend::Stencil {
        // alloc: an error
        return Err(failed(unit, format!("copy-and-patch code of a unit for {:?}", unit.backend)));
    }
    let Some(mut stencils) = target.stencil() else {
        // alloc: an error
        return Err(failed(
            unit,
            format!("copy-and-patch code for {}, which has no stencils", target.triple()),
        ));
    };
    stencils.counts = counted;
    crate::with_context(func, |context| {
        let pairs = stencil_ir(&mut context.func);
        let func = &context.func;
        let mut code =
            blaze_stencil::compile_to(func, &stencils, |code| stencil_code(code, func, named))
                .map_err(|e| failed(unit, e))?
                .map_err(|e| failed(unit, e))?;
        if let Some(pairs) = pairs {
            pairs.hold(context, &mut code).map_err(|e| failed(unit, e))?;
        }
        Ok(code)
    })
}

/// Stencil code as a [`fragment::Code`], copied out of the compile's frame once: its
/// relocations by the names Cranelift's would have (the counter cells by
/// `COUNTER_NAMESPACE`, as `tier::instrument` names them in IR), its unwind information
/// with its landing pads, its lines and where its named slots are.
fn stencil_code(
    code: &blaze_stencil::CodeRef<'_>,
    func: &ir::Function,
    named: &[ir::StackSlot],
) -> Result<fragment::Code, String> {
    use blaze_stencil::code::Name;
    let mut relocs = crate::spare::relocs();
    for reloc in code.relocs {
        let name = match reloc.name {
            Name::User { namespace, index } => fragment::RelocName::user(namespace, index),
            Name::LibCall(call) => fragment::RelocName::LibCall(call),
            Name::Known(symbol) => fragment::RelocName::Known(symbol),
            Name::Own(offset) => fragment::RelocName::Own(offset),
            Name::Counter => fragment::RelocName::User {
                namespace: blaze_stencil::tier::COUNTER_NAMESPACE,
                index: 0,
            },
        };
        let row = fragment::Reloc {
            offset: reloc.at,
            kind: reloc.kind,
            name,
            addend: reloc.addend,
            elf_type: None,
        };
        // alloc: an error
        relocs.push(row).map_err(|e| e.to_string())?;
    }
    let mut lines = crate::spare::lines();
    LineRow::extend_ranges(&mut lines, code.lines.iter().copied());
    Ok(fragment::Code {
        // clone: the function's signature, kept with its code as Cranelift's emit keeps it
        signature: func.signature.clone(),
        // alloc: the function's code, kept by the build
        bytes: code.bytes.to_vec(),
        align: u64::from(code.align),
        relocs,
        unwind: FunctionUnwind::of_stencil(code)?,
        lines,
        slots: stencil_slots(code.slots, named)?,
    })
}

/// Where the named slots of stencil code are, as [`SlotPlaces::of_frame`] gives Cranelift's:
/// from the frame pointer, which the stencils' frame always has.
fn stencil_slots(
    slots: blaze_stencil::code::SlotsRef<'_>,
    named: &[ir::StackSlot],
) -> Result<SlotPlaces, String> {
    // A frame base no variable is told from would be read by nothing.
    if named.is_empty() {
        return Ok(SlotPlaces::default());
    }
    let Some(base) = u8::try_from(slots.base).ok().filter(|it| *it < 32) else {
        // alloc: an error
        return Err(format!(
            "stack slots from DWARF register {}, which `DW_OP_reg` has none of",
            slots.base
        ));
    };
    let mut places = Vec::with_capacity(named.len());
    for &slot in named {
        let Some(&offset) = slots.offsets.get(slot.as_u32() as usize) else {
            // alloc: an error
            return Err(format!(
                "{slot} of a named local, which copy-and-patch code has no place of"
            ));
        };
        places.push((slot.as_u32(), VariablePlace::Frame(i64::from(offset))));
    }
    // alloc: one byte, kept with the code
    Ok(SlotPlaces::new(vec![cranelift_codegen::gimli::DW_OP_reg0.0 + base], places))
}

impl<'db> Prepare for Backends<'db> {
    type Frontend = HirFrontend<'db>;
    type Prepared = Prepared;
    type Export = super::release::Export;

    fn prepare(
        &self,
        frontend: &HirFrontend<'db>,
        unit: &Unit,
        spec: &Spec,
        instance: &HirInstance,
        refs: &Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        check_spec(unit, spec)?;
        frontend.with(|db| {
            let walking = frontend.walking(db, instance).map_err(|e| failed(unit, e))?;
            self.prepared(db, &frontend.hashes, unit, spec, (instance, walking), refs)
        })
    }

    fn summarize(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: &mut Prepared,
        refs: &Refs,
    ) -> Result<(Summary, Self::Export), CodegenError> {
        match prepared {
            Prepared::Llvm(function) => super::release::summarize(unit, spec, function, refs),
            Prepared::Cranelift(_) => {
                Err(failed(unit, "a summary of a Cranelift unit: summaries are LLVM's"))
            }
            Prepared::Stencil(_) => {
                Err(failed(unit, "a summary of a stencil unit: summaries are LLVM's"))
            }
        }
    }

    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Prepared,
        refs: &Refs,
        imports: Option<(&Imports, &[Import<'_, Self::Export>])>,
    ) -> Result<Emitted, CodegenError> {
        match prepared {
            Prepared::Cranelift(function) => {
                if imports.is_some() {
                    return Err(failed(unit, "imports for a Cranelift unit: imports are LLVM's"));
                }
                emit_cranelift(unit, function, refs, spec.counted)
            }
            Prepared::Llvm(function) => super::release::emit(unit, spec, *function, refs, imports),
            Prepared::Stencil(function) => {
                if imports.is_some() {
                    return Err(failed(unit, "imports for a stencil unit: imports are LLVM's"));
                }
                emit_stencil(unit, function, refs, spec.counted)
            }
        }
    }

    // Counting code is never `foldable` ([`Backends::prepared`]): a fold would share one
    // function's cells between many.
    fn masked(&self, _: &Unit, prepared: &Prepared, refs: &Refs) -> Option<Masked> {
        match prepared {
            Prepared::Cranelift(function) if function.foldable => {
                super::fold::masked(function, refs)
            }
            Prepared::Cranelift(_) | Prepared::Llvm(_) | Prepared::Stencil(_) => None,
        }
    }

    fn merge(
        &self,
        group: &Group,
        members: Vec<(&Unit, &Spec, Prepared, &Refs)>,
    ) -> Result<Merged, CodegenError> {
        let mut functions = Vec::with_capacity(members.len());
        for (unit, spec, prepared, refs) in members {
            match prepared {
                Prepared::Cranelift(_) if spec.counted => {
                    return Err(failed(unit, "a fold of a counting unit: its cells are its own"));
                }
                Prepared::Cranelift(function) => functions.push((unit, spec, function, refs)),
                Prepared::Llvm(_) => {
                    return Err(failed(unit, "a fold of an LLVM unit: folds are Cranelift's"));
                }
                Prepared::Stencil(_) => {
                    return Err(failed(unit, "a fold of a stencil unit: folds are Cranelift's"));
                }
            }
        }
        let members: Vec<(&Unit, &Spec, &Function, &Refs)> = functions
            .iter()
            .map(|(unit, spec, function, refs)| (*unit, *spec, &**function, *refs))
            // alloc: once a fold group, exact size: borrows of the boxes `functions` owns
            .collect();
        let Some(&(first, first_spec, ..)) = members.first() else {
            return Err(CodegenError {
                unit: group.key(),
                item: ItemKey(ContentId::default()),
                what: "a fold of no functions".to_owned(), // alloc: the error's message
                front: None,
            });
        };
        // The target's addresses, never the ISA's: on wasm the ISA is a native one whose
        // settings the IR is made under, with 64-bit pointers.
        let pointer = unit_target(first, first_spec)?.pointer();
        super::fold::merge(group, &members, pointer, |unit, _, function, ir, refs| {
            emit_ir(unit, function, ir, refs)
        })
    }

    fn compile_static(
        &self,
        frontend: &HirFrontend<'db>,
        spec: &Spec,
        value: &super::HirStatic,
        refs: &Refs,
    ) -> Result<Static, CodegenError> {
        frontend.with(|db| {
            let hashes = &frontend.hashes;
            super::statics::compile(db, hashes, spec.toolchain.arch, value.0, refs).map_err(
                |what| super::undescribed(db, &super::StaticNamed(db, value.0), None, what),
            )
        })
    }
}

impl Compile for Backends<'_> {
    type Mir<'a> = (&'a HirFrontend<'a>, &'a Spec, &'a HirInstance, &'a Refs);

    fn compile(
        &self,
        unit: &Unit,
        (frontend, spec, instance, refs): Self::Mir<'_>,
    ) -> Result<CompiledFunction, CodegenError> {
        let (_, prepared) = self.prepare(frontend, unit, spec, instance, refs)?;
        Ok(self.emit(unit, spec, prepared, refs, None)?.function)
    }
}

#[cfg(test)]
mod features_tests {
    use super::{unit_features, with_enabled};

    fn names(features: &[&str]) -> Vec<String> {
        features.iter().map(|it| (*it).to_owned()).collect()
    }

    /// A `#[target_feature(enable)]` set is the unit's with what it enables: named once, a
    /// baseline feature not at all, and one the unit turns off on again.
    #[test]
    fn a_functions_set_is_its_units_with_what_it_enables() {
        let x86 = crate::Target::of("x86_64-unknown-linux-gnu").expect("a target");
        let set =
            with_enabled(&x86, x86.cpu(), &[], ["avx", "avx2", "sse2"].into_iter()).expect("a set");
        assert_eq!(set.names(), names(&["+avx", "+avx2"]));
        let unit = names(&["+avx", "+popcnt"]);
        let set = with_enabled(&x86, x86.cpu(), &unit, ["avx", "avx2"].into_iter()).expect("a set");
        assert_eq!(set.names(), names(&["+avx", "+avx2", "+popcnt"]));
        let unit = names(&["-popcnt"]);
        let set = with_enabled(&x86, x86.cpu(), &unit, ["popcnt"].into_iter()).expect("a set");
        assert!(set.is_baseline());
        let unit = x86.features_of(x86.cpu(), &names(&["+avx"])).expect("a set");
        let same =
            with_enabled(&x86, x86.cpu(), &names(&["+avx"]), ["avx"].into_iter()).expect("a set");
        assert_eq!(same, unit);
    }

    /// What `#[blaze::multiversion]`'s plan is made against: the target's baseline with the
    /// toolchain's features on and off.
    #[test]
    fn a_units_features_are_its_baselines_with_its_toolchains() {
        let has = |set: &hir_ty::TargetFeatureSet, name: &str| {
            set.contains(hir_ty::TargetFeature::from_name(name).expect(name))
        };
        let x86 = crate::Target::of("x86_64-unknown-linux-gnu").expect("a target");
        let unit = unit_features(&x86, &[]);
        assert!(has(&unit, "sse2") && has(&unit, "fxsr") && !has(&unit, "avx2"));
        let unit = unit_features(&x86, &names(&["+avx", "+avx2", "-x87"]));
        assert!(has(&unit, "sse2") && has(&unit, "avx2") && !has(&unit, "x87"));
        let arm = crate::Target::of("aarch64-unknown-linux-gnu").expect("a target");
        let unit = unit_features(&arm, &names(&["+sve"]));
        assert!(has(&unit, "neon") && has(&unit, "sve") && !has(&unit, "sve2"));
    }
}
