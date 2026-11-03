//! What the compiler appends to a crate's root file, written into it before its unit is
//! identified ([`write`]): a test crate's harness, the `main` and the description of each test
//! that rustc's `--test` adds, with usercode's fork server, as `HirFrontend::harness` writes
//! them. It is in the text and appended module of the crate's own root, an alias of the file
//! on disk ([`Lowered::append_roots`]), so the unit's identity and record key cover it, a
//! harness of another mode is another text (the unit is stale), and the lib beside its test,
//! which has the file itself, is the same unit under `build` and `test`. A proc macro built
//! for wasm32-wasip1 ([`codegen::wasm_macro`]) has the list of its macros and the bridge's glue
//! appended ([`blaze_macro_eval::wasm_glue`]), which its module exports to the host; its
//! macros are found from its def map either way.

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_api::ApiError;
use blaze_ids::radix;
use blaze_units::schema::Mode;
use hir_ty_codegen::v2::HARNESS_MODULE;
use blaze_lower::{Appended, LowerError, Lowered};

use super::{Job, codegen, compile, graph::Waits, lower_error};

/// The harness of each of `units` built with `cfg(test)` (a test, a check of tests), or the
/// glue of each that is a proc macro built for wasm, written into its crate's root: the
/// root's text on disk, then the module. A test with
/// `harness = false` has none and its root is left as it is.
/// Called on the loop's writer as these units become ready, before they are identified, so
/// what they are identified by has the module.
///
/// A crate that reaches a proc macro whose expanders are not loaded yet is left: what its
/// attributes expand to (a `#[tokio::test]`'s `#[test]`, a function of the root) is not listed
/// until they are, and [`reached`] has it written then. Its unit's identity was set before;
/// the module is made from what that identity covers and the unit's mode and panic strategy,
/// which its record key covers, so the key stays true.
///
/// Each harness takes the panic strategy of its profile and the mode of its codegen policy
/// ([`codegen::policy`]), which a unit's build is compiled with. Two units of one crate that
/// want different modules are refused naming both ([`Lowered::append_roots`]), as is a module
/// that can't be made, naming its crate. A refusal is that unit's alone: the error lists each
/// unit refused with why, and every other unit's root is written.
///
/// A library's `Doctest` unit has no harness of its own: each of its examples, and its runner,
/// is made into a crate of its own here ([`compile::doctest::expand`]), refused as above.
pub(super) fn write(lowered: &mut Lowered, job: &Job, units: impl IntoIterator<Item = u32>) -> Result<(), Vec<(usize, ApiError)>> {
    let paths = Arc::clone(lowered.paths());
    // alloc: once a write, the units it is given
    let units: Vec<u32> = units.into_iter().collect();
    let appended = units.iter().copied().filter(|&unit| tested(job, unit as usize) || codegen::wasm_macro(job, unit as usize));
    let made = lowered.append_roots(appended, |db, unit, krate| {
        if !blaze_lower::macros_loaded(db, krate) {
            return Ok(None);
        }
        let at = || match job.unit(unit) {
            // alloc: an error's place
            Ok(it) => format!("`{}` of `{}`", it.target.name, it.pkg_id),
            Err(error) => error.what,
        };
        let refused = |what: String| LowerError { what, at: Some(at()) };
        if codegen::wasm_macro(job, unit) {
            let glue = blaze_macro_eval::wasm_glue(db, krate).map_err(refused)?;
            return Ok(Some(Appended { after: glue, module: blaze_macro_eval::GLUE_MODULE }));
        }
        let toolchain = codegen::toolchain(job, unit).map_err(refused)?;
        let policy = codegen::policy(job, unit, toolchain).map_err(refused)?;
        let frontend = codegen::frontend_over(db, &paths, &job.mounted, codegen::build_time(job, unit));
        let harness = frontend.harness(krate, policy.profile.panic, policy.mode, blaze_usercode::test::fork_server);
        let Some(harness) = harness.map_err(|it| refused(it.what))? else { return Ok(None) };
        // The module the harness is written in, by its name in the crate's root: what the
        // front end keeps of a root that `#![cfg]` takes away.
        Ok(Some(Appended { after: harness, module: HARNESS_MODULE }))
    });
    // alloc: an error, once per unit refused
    let mut refused: Vec<(usize, ApiError)> = match made {
        Ok(_) => Vec::new(),
        Err(refused) => refused.into_iter().map(|(unit, error)| (unit, lower_error(error))).collect(),
    };
    // A library's doctests: each example and the runner a crate of its own
    // ([`compile::doctest::expand`]).
    for unit in units.into_iter().map(|it| it as usize) {
        if job.unit(unit).is_ok_and(|it| it.mode == Mode::Doctest)
            && let Err(error) = compile::doctest::expand(lowered, job, unit)
        {
            refused.push((unit, lower_error(error)));
        }
    }
    match refused.is_empty() {
        true => Ok(()),
        false => Err(refused),
    }
}

/// The units [`write`] refused, each with why, as one error naming every one: what a write
/// with no unit to charge a refusal to (the lowering's) fails with.
pub(super) fn refused(refused: &[(usize, ApiError)]) -> ApiError {
    let at = refused.first().and_then(|(_, error)| error.at.clone()); // clone: an error, once
    let each = refused
        .iter()
        .map(|(_, error)| match &error.at {
            Some(at) => format!("{} (at {at})", error.what), // alloc: an error, once per unit
            None => error.what.clone(),                     // clone: an error, once per unit
        })
        .collect::<Vec<_>>() // alloc: an error, once
        .join("; ");
    ApiError::new(format!("writing the crate roots' harnesses: {each}"), at) // alloc: an error, once
}

/// The units whose harness one apply writes: `ready`, the units its runs made ready, then
/// every unit that is ready and reaches one of the proc macros `loaded` (their units, just
/// applied) through its dependents, which those macros complete ([`write`]), each once. A
/// unit not ready yet is written when it is.
pub(super) fn reached(job: &Job, waits: &Waits, loaded: &[u32], ready: &[u32]) -> Vec<u32> {
    // alloc: once an apply, the units it writes harnesses of
    let mut written: Vec<u32> = ready.to_vec();
    if loaded.is_empty() {
        return written;
    }
    // alloc: once an apply that loaded proc macros, the units they reach
    let mut seen = vec![false; job.graph.len()];
    for &unit in ready {
        seen[unit as usize] = true;
    }
    let mut stack: Vec<u32> = loaded.to_vec(); // alloc: as `seen`
    let mut reached = Vec::new(); // alloc: as `seen`
    let mut visited = vec![false; job.graph.len()]; // alloc: as `seen`
    while let Some(at) = stack.pop() {
        for &up in job.graph.dependents(at) {
            if !std::mem::replace(&mut visited[up as usize], true) {
                stack.push(up);
                if waits.ready(up) && !seen[up as usize] {
                    reached.push(up);
                }
            }
        }
    }
    radix::sort_by_key(&mut reached, |&it| u64::from(it));
    written.extend(reached);
    written
}

/// Whether `unit`'s crate is built with `cfg(test)`, as blaze-lower makes it: a test, or a check
/// of one (`cargo check --tests`).
fn tested(job: &Job, unit: usize) -> bool {
    job.unit(unit).is_ok_and(|it| it.mode == Mode::Test) || job.plan(unit).is_ok_and(|it| it.checked_as_test)
}
