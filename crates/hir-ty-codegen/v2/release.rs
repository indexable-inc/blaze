//! Release units (docs/v2-codegen.md, "Release"; #899, #176): LLVM per function, with what
//! cross-function optimization needs carried by summaries.
//!
//! [`summarize`] lowers an LLVM unit's IR and runs ThinLTO's pre-link pipeline on it once:
//! the module is its [`Export`], what importers link in, and what LLVM then says of it is its
//! [`Summary`]. [`blaze_codegen::imports::decide`] gives each unit its [`Imports`] from all
//! of them, and [`emit`] compiles the unit from the same pre-linked module with the exports
//! it imports linked in and the whole program's facts applied, through the post-link
//! pipeline of its level. One function per compile: imports are shared through the
//! summaries, never by compiling functions together (v1's units, `RA_JIT_UNITS`).

#![forbid(unsafe_code)]

#[cfg(feature = "llvm")]
use std::sync::Arc;

use blaze_codegen::{
    CodegenError, Unit,
    backend::{Emitted, Import},
    settings::Spec,
    summary::{Imports, Summary},
    template::Refs,
};
#[cfg(feature = "llvm")]
use blaze_codegen::{
    Backend, InstanceKey, ProfileRecord, Profiled, Target,
    settings::{DebugInfo, Pgo},
    summary::{self, Arg, Call},
};
#[cfg(feature = "llvm")]
use blaze_items::CodeHash;
#[cfg(feature = "llvm")]
use cranelift_module::{DataId, FuncId};

use super::{Function, failed};
#[cfg(feature = "llvm")]
use super::{Built, DataPlace, Places};
#[cfg(feature = "llvm")]
use crate::{
    ConstBlob,
    backend::{Optimization, Passes, Selection},
    fragment::{DataRequest, FuncTarget, Requests, Wants},
    llvm::{
        pgo,
        release::{self as llvm, Given, Prelinked},
    },
};

/// Gives the process the profile `bytes`, read from the store under `id`, for the release
/// units whose `Pgo::Use` names it, while the profile is held ([`pgo::give`]).
#[cfg(feature = "llvm")]
pub fn give_profile(id: blaze_ids::ContentId, bytes: &[u8]) -> Result<pgo::Profile, String> {
    pgo::give(id, bytes)
}

/// The content ID a profile's bytes are named by in `Pgo::Use` and the store.
#[cfg(feature = "llvm")]
pub fn profile_id(bytes: &[u8]) -> blaze_ids::ContentId {
    pgo::profile_id(bytes)
}

/// What [`summarize`] leaves in an LLVM unit's prepared function ([`Function::release`]) for
/// its own [`emit`]: its pre-linked module, so that LLVM's pre-link pipeline runs once per
/// function. It goes with the function, so no table is shared between units.
#[derive(Default)]
pub(crate) struct Kept {
    #[cfg(feature = "llvm")]
    own: Option<Arc<[u8]>>,
    /// What its pre-link said: its profile has a record of it with another hash.
    #[cfg(feature = "llvm")]
    mismatched: bool,
}

/// A release function as its importers link it, held for one build.
#[cfg(feature = "llvm")]
pub struct Export {
    /// Its pre-linked module without a line table, with its requests and signature; `None`
    /// for one nothing can import (assembly, data that points at data after it), whose
    /// summary says [`summary::Hint::Never`].
    module: Option<Arc<crate::imports::Export>>,
    /// What each of its requests is among its own references.
    places: Places,
    /// Its code hash: what a compile that imports it built in.
    code: CodeHash,
}

#[cfg(not(feature = "llvm"))]
pub struct Export(std::convert::Infallible);

/// The summary and export of an LLVM unit, from its prepared IR.
#[cfg(feature = "llvm")]
pub(crate) fn summarize(
    unit: &Unit,
    spec: &Spec,
    function: &mut Function,
    refs: &Refs,
) -> Result<(Summary, Export), CodegenError> {
    let _span = tracing::info_span!(target: "blaze", "release summarize").entered();
    let optimization = optimization(unit, spec)?;
    let places = &function.places;
    let pending = match &function.code {
        Built::Ir(pending) => pending,
        Built::Assembled(code, requests) => {
            let params = u32::try_from(code.signature.params.len())
                .map_err(|_| failed(unit, "a function of more than 2^32 parameters"))?;
            let summary = Summary {
                key: unit.instance.key(),
                size: 0,
                hint: summary::Hint::Never,
                // alloc: empty, allocates nothing
                calls: Vec::new(),
                // Its code calls what it calls with whatever it likes.
                addressed: keys(unit, places, refs, 0..requests.funcs.len() as u32)?,
                params,
                // alloc: empty, allocates nothing
                unused: Vec::new(),
                attrs: Default::default(),
                body: Default::default(),
                external: true,
            };
            // clone: the export is held for the build; the function keeps its own to emit by
            let export = Export { module: None, places: places.clone(), code: unit.instance.code };
            return Ok((summary, export));
        }
    };
    let Prelinked { own, export, size, facts, mismatched } = prelinked(unit, spec, function, refs, optimization)?;
    function.release = Kept { own: Some(own), mismatched };
    let requests = &pending.wants.requests;
    let mut calls: Vec<Call> = Vec::with_capacity(facts.calls.len());
    for mut called in facts.calls {
        let Some(callee) = key(unit, places, refs, called.request)? else {
            return Err(failed(unit, "LLVM calls a request that is not a function of the build"));
        };
        // An `asm!` function reads the registers its text names, so its summary counts no
        // parameters (`assembled`) and no argument of a call of it is one to specialize on.
        let request = requests.funcs.get(called.request as usize);
        if request.is_some_and(|it| matches!(it.target, FuncTarget::Asm(_))) {
            called.args.clear();
        }
        match calls.iter_mut().find(|it| it.callee == callee) {
            Some(call) => {
                call.count += called.count;
                let len = call.args.len().min(called.args.len());
                call.args.truncate(len);
                for (known, arg) in call.args.iter_mut().zip(called.args) {
                    if *known != arg {
                        *known = Arg::Unknown;
                    }
                }
            }
            None => calls.push(Call { callee, count: called.count, args: called.args }),
        }
    }
    // What data points at is called from where no summary says: a vtable's methods.
    let in_data = requests
        .datas
        .iter()
        .flat_map(|it| match it {
            DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => &blob.fn_relocs[..],
            _ => &[][..],
        })
        .map(|it| it.1.as_u32());
    let addressed = keys(unit, places, refs, facts.addressed.iter().copied().chain(in_data))?;
    // An instrumented body counts into counters of its own, which an importer's copy has not.
    let importable = requests.ordered() && spec.pgo != Pgo::Generate;
    let hint = if importable { function.hint } else { summary::Hint::Never };
    let summary = Summary {
        key: unit.instance.key(),
        size: u32::try_from(size).unwrap_or(u32::MAX),
        hint,
        calls,
        addressed,
        params: facts.params,
        unused: facts.unused,
        attrs: facts.attrs,
        body: facts.body,
        // Until the build says what code without a summary reaches
        // (`blaze_codegen::imports::close`).
        external: true,
    };
    let module = importable.then(|| {
        // alloc: one a function, held for the build and shared by every compile that imports it
        Arc::new(crate::imports::Export {
            bitcode: export,
            requests: copied(requests),
            // clone: the export's own; the function keeps its IR's to emit
            signature: pending.func.signature.clone(),
            size,
            hint: function.hint,
        })
    });
    // clone: the export is held for the build; the function keeps its own to emit by
    Ok((summary, Export { module, places: places.clone(), code: unit.instance.code }))
}

#[cfg(not(feature = "llvm"))]
pub(crate) fn summarize(
    unit: &Unit,
    _: &Spec,
    _: &mut Function,
    _: &Refs,
) -> Result<(Summary, Export), CodegenError> {
    Err(failed(unit, "blaze was built without `--features llvm`"))
}

/// Compiles an LLVM unit, with what it imports when it is a release unit. `refs` are its own
/// references, then those of what it imports ([`blaze_codegen::backend::release_refs`]).
#[cfg(feature = "llvm")]
pub(crate) fn emit(
    unit: &Unit,
    spec: &Spec,
    mut function: Function,
    refs: &Refs,
    imports: Option<(&Imports, &[Import<'_, Export>])>,
) -> Result<Emitted, CodegenError> {
    let _span = tracing::info_span!(target: "blaze", "release emit").entered();
    let optimization = optimization(unit, spec)?;
    if function.places.refs as usize > refs.len() {
        return Err(failed(
            unit,
            // alloc: an error
            format!("{} references given for a function prepared against {}", refs.len(), function.places.refs),
        ));
    }
    let empty = Imports::default();
    let (decided, imported) = imports.unwrap_or((&empty, &[]));
    if decided.imported.len() != imported.len()
        || std::iter::zip(&decided.imported, imported).any(|(key, it)| *key != it.key)
    {
        return Err(failed(unit, "its imports are not those it was decided to have"));
    }
    if let Built::Assembled(..) = &function.code
        && !imported.is_empty()
    {
        return Err(failed(unit, "an assembly function is given imports"));
    }
    if let Built::Assembled(code, requests) = function.code {
        let (places, spans) = (&function.places, &function.spans);
        let (variables, scopes) = (&function.variables, &function.scopes);
        return emitted(unit, places, spans, variables, scopes, code, &requests, refs);
    }
    // Nothing summarized a unit compiled without imports, or one whose summary the store had.
    let (own, mismatched) = match function.release.own.take() {
        Some(own) => (own, function.release.mismatched),
        None => {
            let prelinked = prelinked(unit, spec, &function, refs, optimization)?;
            (prelinked.own, prelinked.mismatched)
        }
    };
    let features = function.features;
    let Function { code: Built::Ir(pending), places, .. } = &mut function else {
        return Err(failed(unit, "an assembly function has no IR for LLVM"));
    };
    // An import at depth is called by another import's body, whose calls are the compile's
    // only once it is merged, and the imports are in the order of their keys: every one is
    // merged before any is looked for.
    // alloc: once per compile with imports, an entry an import
    let mut merged = Vec::with_capacity(imported.len());
    for import in imported {
        let Some(module) = &import.export.module else {
            // alloc: an error
            return Err(failed(unit, format!("imports {}, which has no LLVM export", import.key.0)));
        };
        let (funcs, datas) =
            merge(unit, &mut pending.wants, places, refs, &module.requests, &import.export.places, import.refs)?;
        merged.push((import, module, funcs, datas));
    }
    let mut linked = Vec::with_capacity(imported.len());
    let mut inlined = Vec::with_capacity(imported.len());
    for (import, module, funcs, datas) in merged {
        let id = requests_of(places, refs, import.key).next().ok_or_else(|| {
            let callers: Vec<String> = imported
                .iter()
                .filter(|it| requests_of(&it.export.places, it.refs, import.key).next().is_some())
                // alloc: an error's, each a caller's name
                .map(|it| it.key.0.to_string())
                .collect();
            let why = if callers.is_empty() {
                // alloc: an error's
                format!(
                    "none of its {} imports' bodies calls it, though the summaries its imports were decided by do",
                    imported.len()
                )
            } else {
                // alloc: an error's
                format!("the bodies of {} call it, and merging them lost the call", callers.join(", "))
            };
            // alloc: an error
            failed(unit, format!("imports {}, which nothing it builds calls: {why}", import.key.0))
        })?;
        // clone: a count of the `Arc`, one an import; LLVM's import holds the export it links
        linked.push(crate::imports::Import { export: module.clone(), id, funcs, datas });
        // A body LLVM did not build in can still have told it what the function returns or
        // that it writes nothing: every import is what the code is made of.
        inlined.push(import.export.code);
    }
    places.refs = u32::try_from(refs.len()).map_err(|_| failed(unit, "more than 2^32 references"))?;
    // clone: `Given` owns what LLVM is told; the decision is the build's, borrowed
    let mut given = Given { constants: decided.constants.clone(), ..Given::default() };
    for (callee, attrs) in &decided.callees {
        given.callees.extend(requests_of(places, refs, *callee).map(|it| (it, *attrs)));
    }
    for (callee, params) in &decided.dead {
        // clone: `Given` owns each request's dead parameters; the decision is borrowed
        given.dead.extend(requests_of(places, refs, *callee).map(|it| (it, params.clone())));
    }
    let signature = &pending.func.signature;
    let name = (spec.pgo == Pgo::Generate).then(|| pgo::name(unit.item, unit.instance.args));
    let (code, covered, counted) = llvm::compile(
        &mut pending.wants,
        &own,
        signature,
        &linked,
        &given,
        optimization,
        &spec.sanitize,
        name.as_deref(),
        features,
    )
    // alloc: an error
    .map_err(|why| failed(unit, format!("LLVM has no code for it: {why}")))?;
    let profile = profiled(unit, spec, counted, mismatched)?;
    placed_since(unit, &pending.wants, places, refs)?;
    let code = crate::backend::pad(code).map_err(|why| failed(unit, why))?;
    let Built::Ir(pending) = &function.code else {
        return Err(failed(unit, "an assembly function has no IR for LLVM"));
    };
    let Function { places, spans, variables, scopes, .. } = &function;
    let mut emitted = emitted(unit, places, spans, variables, scopes, code, &pending.wants.requests, refs)?;
    // What the IR built in (MIR-inlined, `#[inline(always)]` of no member) is the code's too.
    inlined.extend_from_slice(&function.inlined);
    inlined.sort_unstable();
    inlined.dedup();
    emitted.function.inlined = inlined;
    emitted.function.coverage = coverage(spec, covered);
    emitted.function.profile = profile;
    Ok(emitted)
}

/// What SanitizerCoverage made for a function, as its compiled function has it: `None` where
/// the build has no SanitizerCoverage.
#[cfg(feature = "llvm")]
fn coverage(spec: &Spec, covered: crate::llvm::sanitize::Covered) -> Option<blaze_codegen::Covered> {
    let crate::llvm::sanitize::Covered { arrays: [counters, flags, guards], pcs } = covered;
    spec.sanitize.coverage.is_on().then_some(blaze_codegen::Covered { counters, flags, guards, pcs })
}

/// `code` against `refs`, by the function's `places`, as the Cranelift backend converts its
/// own (`super::backend::emit_cranelift`): its unwind information and lines come with it, and
/// its side table of `variables` in `scopes`. `spans` are what its line rows name.
#[cfg(feature = "llvm")]
fn emitted(
    unit: &Unit,
    places: &Places,
    spans: &[blaze_format::SpanRef],
    variables: &[super::side::Variable],
    scopes: &super::side::Scopes,
    code: crate::fragment::Code,
    requests: &Requests,
    refs: &Refs,
) -> Result<Emitted, CodegenError> {
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    let isa: &dyn cranelift_codegen::isa::TargetIsa = &**crate::target().map_err(|e| failed(unit, e))?;
    let (converted, extra) = super::places::resolve_owning(places, refs, |resolve| {
        let side = super::side::of(variables, scopes, &code, resolve.frame())?;
        let converted = super::convert::function(
            resolve.frame(),
            unit.instance.key(),
            unit.arch,
            isa,
            code,
            requests,
            resolve,
            spans,
            side.as_ref().map(|it| it.1),
            // LLVM compiles no wasm32 unit (`backend::check_built`).
            &super::wasm::Header::default(),
        )?;
        Ok::<_, String>((converted, side))
    });
    let (converted, side) = converted.map_err(|e| failed(unit, e))?;
    Ok(Emitted { function: converted, side: side.map(|it| it.0), extra })
}

#[cfg(not(feature = "llvm"))]
pub(crate) fn emit(
    unit: &Unit,
    _: &Spec,
    _: Function,
    _: &Refs,
    _: Option<(&Imports, &[Import<'_, Export>])>,
) -> Result<Emitted, CodegenError> {
    Err(failed(unit, "blaze was built without `--features llvm`"))
}

/// The optimization a unit's settings ask of LLVM.
#[cfg(feature = "llvm")]
fn optimization(unit: &Unit, spec: &Spec) -> Result<Optimization, CodegenError> {
    let Backend::Llvm { opt } = spec.backend else {
        return Err(failed(unit, "a unit Cranelift compiles is given to LLVM"));
    };
    if spec.toolchain.llvm.is_none() {
        return Err(failed(unit, "a unit asks for LLVM, and the toolchain has none"));
    }
    let passes = match opt {
        0 => Passes::None,
        1 => Passes::Less,
        2 => Passes::Default,
        3 => Passes::Aggressive,
        // alloc: an error
        _ => return Err(failed(unit, format!("LLVM has no level {opt}"))),
    };
    let selection = if passes == Passes::None { Selection::Fast } else { Selection::Optimized };
    Ok(Optimization { passes, selection })
}

/// The function's module after the pre-link pipeline.
#[cfg(feature = "llvm")]
fn prelinked(
    unit: &Unit,
    spec: &Spec,
    function: &Function,
    refs: &Refs,
    optimization: Optimization,
) -> Result<Prelinked, CodegenError> {
    let Built::Ir(pending) = &function.code else {
        return Err(failed(unit, "an assembly function has no IR for LLVM"));
    };
    let places = &function.places;
    let instances = |request: u32| {
        let target = places.funcs.get(request as usize).and_then(|&it| refs.get(it as usize));
        matches!(target, Some(Target::Function(_) | Target::Direct(_)))
    };
    let debug = spec.debuginfo != DebugInfo::None;
    let wanted = pgo::Wanted::of(spec.pgo);
    let name = wanted.map(|_| pgo::name(unit.item, unit.instance.args));
    llvm::prelink(
        &pending.wants,
        &pending.func,
        &pending.named,
        function.hint,
        debug,
        optimization,
        &instances,
        &spec.sanitize,
        function.unsanitized,
        wanted.zip(name.as_deref()),
        function.features,
    )
    // alloc: an error
    .map_err(|why| failed(unit, format!("LLVM has no code for it: {why}")))
}

/// What PGO made of the function, as its compiled function has it: its record, instrumented
/// (`counted`, read from its module), or, built with a profile, whether the profile has a
/// record under its name with another hash (`mismatched`, from its pre-link), which the
/// build's `Stats::profile_mismatched` counts. Anything else than `spec.pgo` asked for is an
/// error naming it.
#[cfg(feature = "llvm")]
fn profiled(
    unit: &Unit,
    spec: &Spec,
    counted: Option<pgo::Counted>,
    mismatched: bool,
) -> Result<Option<Profiled>, CodegenError> {
    match (spec.pgo, counted, mismatched) {
        (Pgo::None, None, false) => Ok(None),
        (Pgo::Use(_), None, mismatched) => Ok(Some(Profiled::Used { mismatched })),
        (Pgo::Generate, Some(counted), false) => {
            let pgo::Counted { name_ref, hash, counters, version, .. } = counted;
            let name = pgo::name_bytes(unit.item, unit.instance.args);
            Ok(Some(Profiled::Counted(ProfileRecord { name, name_ref, hash, counters, version })))
        }
        (asked, counted, mismatched) => Err(failed(
            unit,
            // alloc: an error
            format!(
                "built with PGO {asked:?}, LLVM {} it{}",
                if counted.is_some() { "instrumented" } else { "did not instrument" },
                if mismatched { " and its pre-link read a profile of it" } else { "" },
            ),
        )),
    }
}

/// What the function request `request` is in the build: the instance's key of a function
/// of it, `None` for an outside symbol.
#[cfg(feature = "llvm")]
fn key(unit: &Unit, places: &Places, refs: &Refs, request: u32) -> Result<Option<InstanceKey>, CodegenError> {
    let target = places
        .funcs
        .get(request as usize)
        .and_then(|&it| refs.get(it as usize))
        // alloc: an error
        .ok_or_else(|| failed(unit, format!("function request {request} has no place among the references")))?;
    match target {
        Target::Function(key) | Target::Direct(key) => Ok(Some(*key)),
        Target::Extern(_) | Target::Weak(_) => Ok(None),
        // alloc: an error
        other => Err(failed(unit, format!("function request {request} is {other:?}"))),
    }
}

/// The functions of the build `requests` are, sorted, each once.
#[cfg(feature = "llvm")]
fn keys(
    unit: &Unit,
    places: &Places,
    refs: &Refs,
    requests: impl Iterator<Item = u32>,
) -> Result<Vec<InstanceKey>, CodegenError> {
    let mut keys = Vec::with_capacity(requests.size_hint().0);
    for request in requests {
        keys.extend(key(unit, places, refs, request)?);
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

/// The function requests that are the build's function `wanted`, in order.
#[cfg(feature = "llvm")]
fn requests_of(places: &Places, refs: &Refs, wanted: InstanceKey) -> impl Iterator<Item = u32> {
    let is = move |place: u32| match refs.get(place as usize) {
        Some(Target::Function(key) | Target::Direct(key)) => *key == wanted,
        _ => false,
    };
    (0..places.funcs.len() as u32).filter(move |&at| is(places.funcs[at as usize]))
}

/// [`requests_of`], gathered.
#[cfg(test)]
#[cfg(feature = "llvm")]
fn by_key(places: &Places, refs: &Refs, wanted: InstanceKey) -> Vec<u32> {
    requests_of(places, refs, wanted).collect()
}

/// Gives a place to each request LLVM's object asked for as it was read back: constants LLVM
/// put beside the code that hold addresses are the function's own data, and a function it
/// calls of its own accord (a libm routine, wide division) is the outside symbol of that
/// name: its place among the unit's references, or [`super::places::OWN`] when they lack it,
/// which `emitted` keeps by name in the template (`Emitted::extra`).
#[cfg(feature = "llvm")]
fn placed_since(unit: &Unit, wants: &Wants, places: &mut Places, refs: &Refs) -> Result<(), CodegenError> {
    for request in &wants.requests.funcs[places.funcs.len()..] {
        let FuncTarget::Symbol(name) = &request.target else {
            return Err(failed(unit, "LLVM asked for a function that is no symbol"));
        };
        if name.is_empty() {
            return Err(failed(unit, "LLVM calls a symbol with no name"));
        }
        let place = refs.iter().position(|it| matches!(it, Target::Extern(it) if it == name));
        places.funcs.push(place.map_or(super::places::OWN, |it| it as u32));
    }
    for request in &wants.requests.datas[places.datas.len()..] {
        match request {
            DataRequest::Blob { .. } => places.datas.push(DataPlace::Own),
            _ => return Err(failed(unit, "LLVM asked for data that is not its own constant")),
        }
    }
    Ok(())
}

#[cfg(feature = "llvm")]
fn copied(requests: &Requests) -> Requests {
    // clone: the export's own, once a function; the function keeps its, which its emit grows
    Requests { funcs: requests.funcs.clone(), datas: requests.datas.clone() }
}

/// Puts an import's `requests`, which are `theirs` among `their_refs`, among what the
/// compile asks for (`wants`, `places`, places into the compile's `refs`): what each of them
/// is in the compile, functions then data. A static, a vtable or a position asked for twice
/// is one object, as is equal read-only data; LLVM takes two globals for two objects.
#[cfg(feature = "llvm")]
fn merge(
    unit: &Unit,
    wants: &mut Wants,
    places: &mut Places,
    refs: &Refs,
    requests: &Requests,
    theirs: &Places,
    their_refs: &Refs,
) -> Result<(Vec<u32>, Vec<u32>), CodegenError> {
    let place_in = |target: &Target| {
        let found = refs.iter().position(|it| it == target).map(|it| it as u32);
        found.ok_or_else(|| {
            // alloc: an error
            failed(unit, format!("an import refers to {target:?}, which the compile's references lack"))
        })
    };
    let theirs_of = |place: u32| {
        their_refs
            .get(place as usize)
            // alloc: an error
            .ok_or_else(|| failed(unit, format!("an import's place {place} is past its references")))
    };
    let mut funcs = Vec::with_capacity(requests.funcs.len());
    for (at, request) in requests.funcs.iter().enumerate() {
        let theirs = *theirs.funcs.get(at).ok_or_else(|| failed(unit, "an import's request without its place"))?;
        let place = place_in(theirs_of(theirs)?)?;
        let named = |id: usize| places.funcs.get(id).and_then(|&it| refs.get(it as usize));
        let before = wants.requests.funcs.len();
        // clone: `Wants::func` takes the target owned; the import's export is shared, borrowed
        let id = wants.func(request.target.clone(), &request.sig).as_u32();
        let id = if id as usize == before {
            places.funcs.push(place);
            id
        } else if named(id as usize) == refs.get(place as usize) {
            id
        } else {
            // The build keys a callee by who asks for it: the instance the compile asks for is
            // another function of the build than the one the import's body calls, and each
            // request is a declaration of its own (`Symbol::Func` of its ID).
            let found = (0..wants.requests.funcs.len())
                .find(|&it| wants.requests.funcs[it] == *request && named(it) == refs.get(place as usize));
            match found {
                Some(it) => it as u32,
                None => {
                    let id = u32::try_from(wants.requests.funcs.len())
                        .map_err(|_| failed(unit, "more than 2^32 function requests"))?;
                    // clone: as above, a request of the import's that the compile has apart
                    wants.requests.funcs.push(request.clone());
                    places.funcs.push(place);
                    id
                }
            }
        };
        funcs.push(id);
    }
    let mut datas: Vec<u32> = Vec::with_capacity(requests.datas.len());
    for (at, request) in requests.datas.iter().enumerate() {
        let place = match theirs.datas.get(at) {
            Some(DataPlace::Place(theirs)) => DataPlace::Place(place_in(theirs_of(*theirs)?)?),
            // clone: an ID but for a `Text`'s bytes; the compile's place is its own
            Some(other) => other.clone(),
            None => return Err(failed(unit, "an import's data without its place")),
        };
        let moved = |blob: &ConstBlob| -> Result<ConstBlob, CodegenError> {
            let relocs = blob.relocs.iter().map(|&(offset, target, addend)| {
                let moved = datas.get(target.as_u32() as usize).copied();
                let moved = moved.ok_or_else(|| failed(unit, "an import's data points at data after it"))?;
                Ok((offset, DataId::from_u32(moved), addend))
            });
            let fn_relocs = blob.fn_relocs.iter().map(|&(offset, target)| {
                let moved = funcs.get(target.as_u32() as usize).copied();
                let moved = moved.ok_or_else(|| failed(unit, "an import's data points at no function"))?;
                Ok((offset, FuncId::from_u32(moved)))
            });
            Ok(ConstBlob {
                // clone: the compile's request owns its blob; the import's export is shared, borrowed
                bytes: blob.bytes.clone(),
                relocs: relocs.collect::<Result<_, CodegenError>>()?,
                fn_relocs: fn_relocs.collect::<Result<_, CodegenError>>()?,
                align: blob.align,
                // clone: as `bytes`
                type_ids: blob.type_ids.clone(),
            })
        };
        let before = wants.requests.datas.len();
        let id = match request {
            DataRequest::Blob { blob, identity } => wants.define_data_as(moved(blob)?, *identity).as_u32(),
            DataRequest::Vtable { ty, trait_ref, blob } => {
                let found = wants.requests.datas.iter().position(|it| {
                    matches!(it, DataRequest::Vtable { ty: other, trait_ref: other_ref, .. }
                        if other == ty && other_ref == trait_ref)
                });
                match found {
                    Some(found) => found as u32,
                    None => {
                        let blob = moved(blob)?;
                        // clone: the compile's request owns its type; the import's export is shared, borrowed
                        wants.requests.datas.push(DataRequest::Vtable { ty: ty.clone(), trait_ref: trait_ref.clone(), blob });
                        before as u32
                    }
                }
            }
            other => match wants.requests.datas.iter().position(|it| it == other) {
                Some(found) => found as u32,
                None => {
                    // clone: the compile's request is its own; the import's export is shared, borrowed
                    wants.requests.datas.push(other.clone());
                    before as u32
                }
            },
        };
        if id as usize == before {
            places.datas.push(place);
        } else if places.datas[id as usize] != place {
            return Err(failed(unit, "an import asks for data the compile has as another"));
        }
        datas.push(id);
    }
    Ok((funcs, datas))
}

#[cfg(all(test, feature = "llvm"))]
mod tests {
    use blaze_codegen::{Arch, Instance, Settings, settings::Toolchain};
    use blaze_ids::ContentId;
    use blaze_items::ItemKey;
    use cranelift_codegen::{ir::Signature, isa::CallConv};

    use super::*;
    use crate::DataIdentity;

    fn unit() -> Unit {
        Unit {
            instance: Instance {
                code: CodeHash(ContentId([1; 32])),
                args: ContentId([2; 32]),
                settings: Settings(ContentId([3; 32])),
            },
            backend: Backend::Llvm { opt: 3 },
            arch: Arch::X86_64,
            item: ItemKey(ContentId([4; 32])),
        }
    }

    fn instance(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn symbol(wants: &mut Wants, name: &str) -> u32 {
        let signature = Signature::new(CallConv::SystemV);
        wants.func(FuncTarget::Symbol(name.to_owned()), &signature).as_u32()
    }

    fn blob(bytes: &[u8]) -> ConstBlob {
        ConstBlob { bytes: bytes.to_vec(), align: 8, ..ConstBlob::default() }
    }

    /// The importer asks for `a` and the bytes 1 2 3; the import for `b`, `a`, the same
    /// bytes, bytes that point at them and at `b`, and a location of nowhere.
    #[test]
    fn an_imports_requests_are_the_compiles_each_once() {
        let isa = crate::target().unwrap();
        let mut wants = Wants::new(&**isa);
        symbol(&mut wants, "a");
        wants.define_data_as(blob(&[1, 2, 3]), DataIdentity::Shared);
        let mut places = Places { funcs: vec![0], datas: vec![DataPlace::Own], refs: 1 };

        let mut theirs = Wants::new(&**isa);
        let b = symbol(&mut theirs, "b");
        symbol(&mut theirs, "a");
        let same = theirs.define_data_as(blob(&[1, 2, 3]), DataIdentity::Shared);
        let mut pointing = blob(&[0; 16]);
        pointing.relocs = vec![(0, same, 0)];
        pointing.fn_relocs = vec![(8, FuncId::from_u32(b))];
        theirs.define_data_as(pointing, DataIdentity::Shared);
        theirs.requests.datas.push(DataRequest::Location(None));
        let their_places = Places {
            funcs: vec![0, 1],
            datas: vec![DataPlace::Own, DataPlace::Own, DataPlace::Nowhere],
            refs: 2,
        };
        let their_refs = [Target::Extern("b".to_owned()), Target::Extern("a".to_owned())];
        let refs = [Target::Extern("a".to_owned()), Target::Extern("b".to_owned())];

        let (funcs, datas) =
            merge(&unit(), &mut wants, &mut places, &refs, &theirs.requests, &their_places, &their_refs)
                .unwrap();
        assert_eq!(funcs, [1, 0]);
        assert_eq!(datas, [0, 1, 2]);
        assert_eq!(places.funcs, [0, 1]);
        assert_eq!(places.datas, [DataPlace::Own, DataPlace::Own, DataPlace::Nowhere]);
        let DataRequest::Blob { blob: moved, .. } = &wants.requests.datas[1] else { panic!() };
        assert_eq!(moved.relocs, [(0, DataId::from_u32(0), 0)]);
        assert_eq!(moved.fn_relocs, [(8, FuncId::from_u32(1))]);
        // The location is one request, however many bodies ask for it.
        let again =
            merge(&unit(), &mut wants, &mut places, &refs, &theirs.requests, &their_places, &their_refs)
                .unwrap();
        assert_eq!(again, (funcs, datas));
        assert_eq!(wants.requests.datas.len(), 3);
    }

    /// The importer's request of `a` is the compile's function 5; the import's same request
    /// is its function 6, as the build keys a callee by who asks for it: two requests, the
    /// second made once however many bodies ask for it.
    #[test]
    fn one_request_that_is_two_functions_is_two_requests() {
        let isa = crate::target().unwrap();
        let mut wants = Wants::new(&**isa);
        symbol(&mut wants, "a");
        let mut places = Places { funcs: vec![0], datas: Vec::new(), refs: 2 };
        let mut theirs = Wants::new(&**isa);
        symbol(&mut theirs, "a");
        let their_places = Places { funcs: vec![0], datas: Vec::new(), refs: 1 };
        let their_refs = [Target::Direct(instance(6))];
        let refs = [Target::Direct(instance(5)), Target::Direct(instance(6))];
        let mut merged =
            || merge(&unit(), &mut wants, &mut places, &refs, &theirs.requests, &their_places, &their_refs).unwrap();
        assert_eq!(merged().0, [1]);
        assert_eq!(merged().0, [1]);
        assert_eq!(places.funcs, [0, 1]);
        assert_eq!(wants.requests.funcs.len(), 2);
    }

    #[test]
    fn an_import_that_refers_to_what_the_compile_lacks_is_refused() {
        let isa = crate::target().unwrap();
        let mut wants = Wants::new(&**isa);
        let mut places = Places::default();
        let mut theirs = Wants::new(&**isa);
        symbol(&mut theirs, "c");
        let their_places = Places { funcs: vec![0], datas: Vec::new(), refs: 1 };
        let refused = merge(
            &unit(),
            &mut wants,
            &mut places,
            &[Target::Extern("a".to_owned())],
            &theirs.requests,
            &their_places,
            &[Target::Extern("c".to_owned())],
        )
        .unwrap_err();
        assert!(refused.what.contains("lack"), "{}", refused.what);
    }

    #[test]
    fn requests_are_keys_by_their_places() {
        let places = Places { funcs: vec![1, 0, 2, 1], datas: Vec::new(), refs: 3 };
        let refs = [Target::Function(instance(5)), Target::Direct(instance(6)), Target::Extern("x".to_owned())];
        assert_eq!(key(&unit(), &places, &refs, 0).unwrap(), Some(instance(6)));
        assert_eq!(key(&unit(), &places, &refs, 2).unwrap(), None);
        assert!(key(&unit(), &places, &refs, 4).is_err());
        assert_eq!(keys(&unit(), &places, &refs, 0..4).unwrap(), [instance(5), instance(6)]);
        assert_eq!(by_key(&places, &refs, instance(6)), [0, 3]);
    }

    #[test]
    fn what_llvm_asks_for_on_its_own_is_placed() {
        let isa = crate::target().unwrap();
        let mut wants = Wants::new(&**isa);
        symbol(&mut wants, "a");
        let mut places = Places { funcs: vec![0], datas: Vec::new(), refs: 2 };
        symbol(&mut wants, "fmod");
        wants.define_data_as(blob(&[7]), DataIdentity::Shared);
        let refs = [Target::Extern("a".to_owned()), Target::Extern("fmod".to_owned())];
        placed_since(&unit(), &wants, &mut places, &refs).unwrap();
        assert_eq!(places.funcs, [0, 1]);
        assert_eq!(places.datas, [DataPlace::Own]);
        let sincos = symbol(&mut wants, "sincos");
        placed_since(&unit(), &wants, &mut places, &refs).unwrap();
        assert_eq!(places.funcs, [0, 1, super::super::places::OWN]);
        symbol(&mut wants, "");
        assert!(placed_since(&unit(), &wants, &mut places, &refs).is_err());

        // The symbol the references lack is kept by name for LLVM's code only, once however
        // often it is called.
        use super::super::convert::Resolver as _;
        let request = &wants.requests.funcs[sincos as usize];
        let strict = super::super::places::resolve(&places, &refs, |it| it.func(sincos, request));
        assert!(strict.is_err());
        let (targets, extra) = super::super::places::resolve_owning(&places, &refs, |it| {
            [it.func(sincos, request), it.func(0, &wants.requests.funcs[0]), it.symbol("sincos")]
        });
        let [own, listed, again] = targets.map(Result::unwrap);
        assert_eq!(own, Target::Extern("sincos".to_owned()));
        assert_eq!(listed, Target::Extern("a".to_owned()));
        assert_eq!(again, own);
        assert_eq!(extra, ["sincos"]);
    }

    fn spec(opt: u8) -> Spec {
        Spec {
            toolchain: Toolchain {
                id: ContentId([1; 32]),
                target: "x86_64-unknown-linux-gnu".to_owned(),
                arch: Arch::X86_64,
                cpu: "x86-64".to_owned(),
                features: Vec::new(),
                cranelift: "0".to_owned(),
                llvm: Some("23".to_owned()),
                stencils: None,
                frame_pointers: false,
                relocation: blaze_codegen::settings::Relocation::Pic,
            },
            backend: Backend::Llvm { opt },
            overflow_checks: false,
            debug_assertions: false,
            panic: blaze_codegen::settings::Panic::Unwind,
            debuginfo: DebugInfo::None,
            fold: false,
            counted: false,
            mir_inline: false,
            pgo: blaze_codegen::settings::Pgo::None,
            sanitize: blaze_codegen::settings::Sanitize::default(),
        }
    }

    #[test]
    fn levels_are_llvms() {
        assert_eq!(optimization(&unit(), &spec(3)).unwrap().passes, Passes::Aggressive);
        assert_eq!(optimization(&unit(), &spec(2)).unwrap().passes, Passes::Default);
        assert_eq!(optimization(&unit(), &spec(0)).unwrap().selection, Selection::Fast);
        assert!(optimization(&unit(), &spec(4)).is_err());
        let cranelift = Spec { backend: Backend::Cranelift { speed: true }, ..spec(2) };
        assert!(optimization(&unit(), &cranelift).is_err());
        let without = Spec { toolchain: Toolchain { llvm: None, ..spec(2).toolchain }, ..spec(2) };
        assert!(optimization(&unit(), &without).is_err());
    }

    /// An instrumented function keeps LLVM's record under its own name; one built with a
    /// profile says whether the profile missed it, so the build counts it rather than refusing
    /// it; and what `Pgo` did not ask for is an error.
    #[test]
    fn what_pgo_made_is_the_functions() {
        let unit = unit();
        let with = |pgo| Spec { pgo, ..spec(2) };
        let name = pgo::name(unit.item, unit.instance.args);
        let counted =
            || pgo::Counted { name: name.clone(), name_ref: 5, hash: 7, counters: 3, version: 10 };
        let Some(Profiled::Counted(record)) = profiled(&unit, &with(Pgo::Generate), Some(counted()), false).unwrap()
        else {
            panic!("an instrumented function without its record");
        };
        assert_eq!(record.name(), name);
        assert_eq!((record.name_ref, record.hash, record.counters, record.version), (5, 7, 3, 10));

        let used = with(Pgo::Use(ContentId([9; 32])));
        for mismatched in [false, true] {
            assert_eq!(profiled(&unit, &used, None, mismatched).unwrap(), Some(Profiled::Used { mismatched }));
        }
        assert_eq!(profiled(&unit, &with(Pgo::None), None, false).unwrap(), None);

        assert!(profiled(&unit, &with(Pgo::None), Some(counted()), false).is_err());
        assert!(profiled(&unit, &with(Pgo::None), None, true).is_err());
        assert!(profiled(&unit, &with(Pgo::Generate), None, false).is_err());
        assert!(profiled(&unit, &with(Pgo::Generate), Some(counted()), true).is_err());
        assert!(profiled(&unit, &used, Some(counted()), false).is_err());
    }
}