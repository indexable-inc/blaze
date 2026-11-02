//! `blaze pack` ([`crate::serve::work::Front::pack`]): the program's crate and every crate it is
//! made of, packed by [`blaze_wasm_pack::record`] from the job's front end once its check is
//! built: its build scripts run and its proc macros loaded by the daemon in the sandbox, as any
//! check's are. Nothing here loads a workspace or runs anything.

use base_db::Crate;
use blaze_api::ApiError;
use blaze_lower::{Db, Lowered};
use blaze_units::{
    query::Units,
    schema::{Mode, Unit},
};
use hir_def::{
    AssocItemId, DefWithBodyId, ItemContainerId, Lookup as _, ModuleDefId,
    expr_store::Body,
    nameres::{DefMap, crate_def_map},
};

use super::Job;

/// Package `program`'s crate and every crate it is made of, as `blaze_wasm_pack::Pack`'s JSON.
/// The expansions recorded are every one the program's names and the types of its functions'
/// bodies need, which is what the in-browser compiler asks for.
pub(super) fn pack(job: &Job, lowered: &mut Lowered, program: &str) -> Result<Vec<u8>, ApiError> {
    let unit = program_unit(&job.input.units, program)?;
    let Some(krate) = lowered.krate(unit) else {
        // alloc: an error, once
        return Err(ApiError::new(format!("package `{program}`'s crate was not lowered"), Some(program.to_owned())));
    };
    let pack = blaze_wasm_pack::record(lowered.served_db(), krate, |db, crates| {
        for &it in crates {
            crate_def_map(db, it);
        }
        hir_ty::attach_db(db, || {
            for body in bodies(db, krate) {
                hir_ty::InferenceResult::of(db, body);
            }
        });
    })
    // alloc: an error, once
    .map_err(|what| ApiError::new(format!("packing `{program}`: {what}"), Some(program.to_owned())))?;
    // alloc: the pack, once
    serde_json::to_vec(&pack).map_err(|error| ApiError::new(format!("the pack of `{program}` as JSON: {error}"), None))
}

/// The unit of package `program` the pack is of: its binary, or with none its library. A
/// package with more than one binary is refused, naming them.
pub(super) fn program_unit(units: &Units, program: &str) -> Result<usize, ApiError> {
    let named = |unit: &Unit| units.packages.iter().any(|it| it.name == program && it.id == unit.pkg_id);
    let checked = |unit: &Unit| unit.mode == Mode::Check && !unit.checked_as_test;
    let kind = |unit: &Unit, kinds: &[&str]| unit.target.kind.iter().any(|it| kinds.contains(&it.as_str()));
    let candidates = || units.graph.units.iter().enumerate().filter(|(_, it)| named(it) && checked(it));
    let mut bins = candidates().filter(|(_, it)| kind(it, &["bin"]));
    if let Some((at, first)) = bins.next() {
        if let Some((_, second)) = bins.next() {
            let (first, second) = (&first.target.name, &second.target.name);
            let what = format!("package `{program}` has more than one binary (`{first}`, `{second}`): a pack is of one program"); // alloc: an error, once
            return Err(ApiError::new(what, Some(program.to_owned()))); // alloc: an error, once
        }
        return Ok(at);
    }
    let libs = ["lib", "rlib", "dylib", "cdylib", "staticlib"];
    if let Some((at, _)) = candidates().find(|(_, it)| kind(it, &libs)) {
        return Ok(at);
    }
    let what = format!("the check names no binary or library of package `{program}`: ask it with `-p {program}`"); // alloc: an error, once
    Err(ApiError::new(what, Some(program.to_owned()))) // alloc: an error, once
}

/// Every body of `krate`: of its functions, constants and statics, in its modules, its `impl`s
/// and the blocks of each body, each once.
fn bodies(db: &Db, krate: Crate) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new(); // alloc: once per pack
    items(db, crate_def_map(db, krate), &mut bodies);
    let mut at = 0;
    while let Some(&body) = bodies.get(at) {
        at += 1;
        for (_, block) in Body::of(db, body).blocks(db) {
            items(db, block, &mut bodies);
        }
    }
    bodies
}

/// The bodies of the items of `def_map`'s modules and their `impl`s.
fn items(db: &Db, def_map: &DefMap, bodies: &mut Vec<DefWithBodyId>) {
    let outside = |container: ItemContainerId| matches!(container, ItemContainerId::ExternBlockId(_));
    for (_, module) in def_map.modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(id) if !outside(id.lookup(db).container) => bodies.push(id.into()),
                ModuleDefId::StaticId(id) if !outside(id.lookup(db).container) => bodies.push(id.into()),
                ModuleDefId::ConstId(id) => bodies.push(id.into()),
                _ => {}
            }
        }
        bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(id) => bodies.push(id.into()),
                    AssocItemId::ConstId(id) => bodies.push(id.into()),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    }
}
