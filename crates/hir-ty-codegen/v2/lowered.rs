//! What an instance is for a caller that compiles its IR itself: the compiled tier of proc
//! macros (change 20 of docs/v2/plan/design/compile-time.md; its design in
//! docs/v2/plan/lanes/fastcompile/macros-tier.md, "What hir-ty-codegen must emit, and the
//! confining pass"). The tier confines the IR copy-and-patch would compile and compiles it
//! with `blaze_stencil`. The IR is the one [`super::Prepared::Stencil`] holds, made by the
//! same function ([`backend::built_function`]) and lowered by the same pass
//! ([`backend::stencil_ir`]): handed out here instead of emitted. A build never calls this.

use blaze_codegen::{
    Backend, CodegenError, Data, Target, Unit,
    settings::{Relocation, Spec},
    template::Refs,
};
use cranelift_codegen::ir;
use hir_def::attrs::AttrFlags;

use super::{Built, HirFrontend, HirInstance, Kind, backend, convert, failed, places::resolve};
use crate::{Instance, InstanceDef, fragment::FuncTarget};

/// What an instance is for a caller that compiles its IR itself.
pub enum Lowering {
    /// The IR `Backend::Stencil` would compile: after ABI lowering and `float_pair::lower`,
    /// before any emit. Its signature is its code's: a pair of floats `windows_fastcall`
    /// can't return is two integers in it and in its calls alike.
    Ir {
        func: ir::Function,
        /// By the index of each user name `func` declares (`func.params.user_named_funcs()`),
        /// its target in the build, as `convert.rs` maps a relocation at it: namespace 0 a
        /// function, 1 data. No other namespace is in it: `counted` and `fold` are off.
        names: Vec<Target>,
        /// The function's own data, which `names` name by [`Target::Data`]: its constants,
        /// `file!()` texts and panic locations, each pointing only at those before it.
        data: Vec<Data>,
    },
    /// `Kind::Asm`: a `global_asm!`/`naked_asm!` function, by its text.
    Asm(String),
    /// `Kind::Allocator`: a function of the allocator shim, by its symbol (`__rust_alloc`, ...).
    Allocator(&'static str),
}

/// What `instance`, a unit of `spec`, is against `refs` (its references' targets, as
/// [`blaze_codegen::backend::Prepare::prepare`] takes them) for a caller that compiles its
/// IR itself. `spec` is the macro crate's own, with `backend: Backend::Stencil`, `counted`
/// and `fold` off and a static target; any other is refused, naming what and the unit. A
/// function whose body has `asm!` statements, or is a naked function, is an error naming it.
pub fn lowered(
    frontend: &HirFrontend<'_>,
    unit: &Unit,
    spec: &Spec,
    instance: &HirInstance,
    refs: &Refs,
) -> Result<Lowering, CodegenError> {
    check(unit, spec)?;
    match &instance.0 {
        // clone: once an assembly function handed out, its text
        Kind::Asm(text, _) => return Ok(Lowering::Asm(text.clone())),
        Kind::Allocator(forward) => return Ok(Lowering::Allocator(forward.symbol)),
        Kind::Code(..) | Kind::Start(_) => {}
    }
    frontend.with(|db, homes| {
        let refused = |what: &str| {
            let name = super::linked::debug_name(db, &instance.0);
            // alloc: an error
            failed(unit, format!("{name}: {what}, which the compiled tier of proc macros does not run"))
        };
        // Refused before it is built: building a naked function assembles it, and the refusal
        // is the tier's whether or not anything here assembles.
        if let Kind::Code(Instance { def: InstanceDef::Fn(func), .. }, _) = &instance.0
            && AttrFlags::query(db, (*func).into()).contains(AttrFlags::NAKED)
        {
            return Err(refused("a naked function (`naked_asm!`)"));
        }
        let inlined = frontend.inlines(db, instance).map_err(|e| failed(unit, e))?;
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            let walked = (instance, inlined);
            let function =
                backend::built_function(db, homes, &frontend.hashes, unit, spec, walked, refs, frame)?;
            let Built::Ir(pending) = function.code else {
                return Err(refused("a naked function (`naked_asm!`)"));
            };
            let requests = &pending.wants.requests;
            if requests.funcs.iter().any(|it| matches!(it.target, FuncTarget::Asm(_))) {
                return Err(refused("a body with `asm!` statements"));
            }
            let mut func = pending.func;
            // What `hold` puts back is the signature of the code a backend makes, and the
            // caller makes the code of `func`, whose signature it is.
            drop(backend::stencil_ir(&mut func));
            let key = unit.instance.key();
            let (names, data) = resolve(&function.places, refs, |placed| {
                convert::names(placed.frame(), key, unit.arch, &func, requests, placed)
            })
            // alloc: an error
            .map_err(|e| failed(unit, format!("{}: {e}", super::Named(db, instance))))?;
            Ok(Lowering::Ir { func, names, data })
        })
    })
}

/// Whether `spec` is what the compiled tier compiles: copy-and-patch's IR, of code that
/// counts no calls, folds nothing and is static, built by the process as
/// [`backend::check_built`] says.
fn check(unit: &Unit, spec: &Spec) -> Result<(), CodegenError> {
    let refuse = |what: String| {
        // alloc: an error
        failed(unit, format!("the IR of a unit {what}: the compiled tier of proc macros takes \
                              copy-and-patch's IR (Backend::Stencil) of code that counts no \
                              calls, folds nothing and is static"))
    };
    if spec.backend != Backend::Stencil {
        // alloc: an error
        return Err(refuse(format!("for {:?}", spec.backend)));
    }
    if spec.counted {
        // alloc: an error
        return Err(refuse("that counts its calls (`counted`)".to_owned()));
    }
    if spec.fold {
        // alloc: an error
        return Err(refuse("that folds (`fold`)".to_owned()));
    }
    if spec.toolchain.relocation == Relocation::Pic {
        // alloc: an error
        return Err(refuse(format!("of position-independent code for {}", spec.toolchain.target)));
    }
    backend::check_built(unit, spec)
}

#[cfg(test)]
mod tests;
