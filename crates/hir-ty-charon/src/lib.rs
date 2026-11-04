//! blaze's MIR as Charon's LLBC, the input Aeneas reads, without rustc.
//!
//! charon-driver reads a crate's MIR out of rustc and translates it into ULLBC (an unstructured
//! control-flow graph), then runs Charon's passes, which give back LLBC (structured: `loop`,
//! `if`, `match`) and write it as `.llbc` JSON. This crate is the first half in place of the
//! driver: [`translate_crate`] reads blaze's runtime MIR ([`hir_ty::db::HirDatabase::runtime_mir_body`])
//! and builds the same ULLBC shapes the driver builds from rustc's; [`to_llbc`] runs Charon's own
//! passes (`charon_lib::transform`) with the Aeneas preset; [`llbc_json`] writes the file as
//! Charon's export does.
//!
//! Version 1 takes monomorphic, safe code: free functions and inherent methods over `bool`, the
//! integers (with overflow checks), unit, tuples, local structs and enums, and references with
//! late-bound or elided lifetimes; loops, branches, matches, calls, asserts and drops of values
//! without drop glue. Anything else is an [`Unsupported`] naming the function and the construct,
//! never a quiet approximation.
//!
//! One module per concern: [`items`] walks the crate and owns the ids, [`names`] builds Charon's
//! paths, [`types`] the types and type declarations, [`signatures`] the signatures and their
//! regions, [`bodies`] the bodies.

#![feature(allocator_api)]

mod bodies;
mod items;
mod names;
mod signatures;
#[cfg(test)]
mod tests;
mod types;

use std::{
    cell::RefCell,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
};

use base_db::{Crate, Db};
use charon_lib::{
    ast::{Body, TranslatedCrate},
    errors::ErrorCtx,
    export::{CharonVersion, CrateData},
    options::{CliOpts, Preset, TranslateOptions},
    transform::{TransformCtx, run_transformation_passes},
};

/// Why a construct is refused: the rule of version 1 it falls outside of.
pub(crate) mod why {
    pub(crate) const GENERICS: &str = "hir-ty-charon v1 translates monomorphic items only: no \
        type, const or early-bound lifetime parameters, no where clauses, no generic impls";
    pub(crate) const TRAITS: &str =
        "hir-ty-charon v1 translates no traits, trait impls or calls of trait methods";
    pub(crate) const TYPES: &str = "hir-ty-charon v1 translates `bool`, the integers, unit, \
        tuples, `!`, references and local non-generic structs and enums";
    pub(crate) const UNSAFE: &str = "hir-ty-charon v1 translates safe Rust only: no unsafe, raw \
        pointers, statics, inline assembly or extern items";
    pub(crate) const CONTROL: &str = "hir-ty-charon v1 translates loops, branches, matches, \
        direct calls, asserts and returns; no closures, function pointers, coroutines, async or \
        tail calls";
    pub(crate) const ITEMS: &str = "hir-ty-charon v1 translates free functions, inherent \
        methods, structs and enums; every other item of the crate is refused, not skipped";
    pub(crate) const OPERATIONS: &str = "hir-ty-charon v1 translates integer and boolean \
        arithmetic, comparisons, integer casts, borrows, tuples and local structs and enums";
    pub(crate) const DROPS: &str =
        "hir-ty-charon v1 translates drops of values without drop glue only";
    pub(crate) const PHASE: &str = "runtime MIR (`MirPhase::Runtime`) never has it; reaching \
        one means the body is not the one hir-ty-charon was written against";
    pub(crate) const MIR: &str =
        "hir-ty-charon translates the runtime MIR blaze lowers; it has none for this body";
    pub(crate) const TARGET: &str =
        "Charon's layout passes need the target: its tuple and its data layout";
}

/// A construct of a function version 1 does not translate. `function` is the item's path
/// (`krate::module::name`, `krate::module::Type::method` for a method), `construct` what in it
/// was refused, `why` the rule that refused it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Unsupported {
    pub function: String,
    pub construct: String,
    pub why: &'static str,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}`: {} is not translated to LLBC ({})",
            self.function, self.construct, self.why
        )
    }
}

impl std::error::Error for Unsupported {}

/// `Unsupported` for `construct` of `function`, refused by `why`.
pub(crate) fn refuse(
    function: &str,
    construct: impl Into<String>,
    why: &'static str,
) -> Unsupported {
    Unsupported {
        function: function.to_owned(), // alloc: an error, once per refusal
        construct: construct.into(),
        why,
    }
}

/// Why [`to_llbc`] gave no LLBC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The crate has constructs version 1 does not translate, sorted.
    Unsupported(Vec<Unsupported>),
    /// Charon refused its own options (`CliOpts::validate`): the preset changed under us.
    Options(String),
    /// Charon's passes reported errors. `items` are the items they named, `bodies` each
    /// function body left with an error (its path and Charon's message), `unstructured` the
    /// functions whose bodies the passes did not structure. Charon prints its messages to stderr.
    Charon {
        error_count: usize,
        items: Vec<String>,
        bodies: Vec<String>,
        unstructured: Vec<String>,
    },
    /// A pass panicked: the panic's message. Only this crate's job fails.
    Panicked(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(all) => {
                write!(f, "{} construct(s) are not translated to LLBC", all.len())?;
                for it in all {
                    write!(f, "\n  {it}")?;
                }
                Ok(())
            }
            Error::Options(message) => {
                write!(f, "Charon refused the Aeneas preset's options: {message}")
            }
            Error::Charon { error_count, items, bodies, unstructured } => {
                write!(f, "Charon's passes reported {error_count} error(s) (printed on stderr)")?;
                for item in items {
                    write!(f, "\n  in `{item}`")?;
                }
                for body in bodies {
                    write!(f, "\n  body error: {body}")?;
                }
                for item in unstructured {
                    write!(f, "\n  `{item}`: the passes left its body unstructured")?;
                }
                Ok(())
            }
            Error::Panicked(message) => write!(f, "Charon's passes panicked: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// The crate's local functions, and the ADTs and foreign functions they use, as ULLBC: what
/// charon-driver hands Charon's passes. Every construct version 1 refuses is collected, sorted.
pub fn translate_crate(db: &Db, krate: Crate) -> Result<TranslatedCrate, Vec<Unsupported>> {
    blaze_query::db::attach(db, || items::translate(db, krate))
}

/// The options [`to_llbc`] runs Charon's passes with: the Aeneas preset, as `charon
/// --preset=aeneas` sets them.
pub fn aeneas_options() -> Result<CliOpts, Error> {
    let mut options = CliOpts { preset: Some(Preset::Aeneas), ..CliOpts::default() };
    options.apply_preset();
    options.validate().map_err(|err| Error::Options(format!("{err:#}")))?; // alloc: an error
    Ok(options)
}

/// The crate as LLBC: [`translate_crate`], then Charon's passes with [`aeneas_options`].
pub fn to_llbc(db: &Db, krate: Crate) -> Result<TranslatedCrate, Error> {
    let ullbc = translate_crate(db, krate).map_err(Error::Unsupported)?;
    run_passes(ullbc)
}

/// Charon's passes over `translated`, as charon-driver runs them after translation.
fn run_passes(mut translated: TranslatedCrate) -> Result<TranslatedCrate, Error> {
    let options = aeneas_options()?;
    translated.options = options.clone(); // clone: the options the export records
    let ran = catch_unwind(AssertUnwindSafe(move || {
        let mut errors = ErrorCtx::new();
        let transform_options = TranslateOptions::new(&mut errors, &options);
        let mut ctx =
            TransformCtx { options: transform_options, translated, errors: RefCell::new(errors) };
        run_transformation_passes(&options, &mut ctx);
        ctx
    }));
    let ctx = ran.map_err(|payload| Error::Panicked(panic_message(payload.as_ref())))?;

    let fmt_name = |name: &charon_lib::ast::Name| names::display(name);
    let errors = ctx.errors.borrow();
    let mut items = Vec::new(); // alloc: an error report
    for id in ctx.translated.all_ids() {
        if errors.item_has_errors(id)
            && let Some(name) = ctx.translated.item_names.get(&id)
        {
            items.push(fmt_name(name));
        }
    }
    let mut bodies = Vec::new(); // alloc: an error report
    let mut unstructured = Vec::new(); // alloc: an error report
    for decl in ctx.translated.fun_decls.iter() {
        match &decl.body {
            Body::Error(err) => {
                // alloc: an error report
                bodies.push(format!("`{}`: {}", fmt_name(&decl.item_meta.name), err.msg));
            }
            Body::Unstructured(_) => unstructured.push(fmt_name(&decl.item_meta.name)),
            _ => {}
        }
    }
    let error_count = errors.error_count;
    let failed = errors.has_errors() || !bodies.is_empty() || !unstructured.is_empty();
    drop(errors);
    if failed {
        return Err(Error::Charon { error_count, items, bodies, unstructured });
    }
    let mut translated = ctx.translated;
    renumber_llbc_ids(&mut translated);
    Ok(translated)
}

/// Charon draws LLBC block and statement ids from process-wide counters
/// (`BlockId::fresh`, `StatementId::fresh`), so the ids, and the JSON, depend on everything
/// translated before in the process and on thread timing. This numbers them again from zero in
/// the crate's order (functions by id, each body in pre-order); one map per kind keeps every
/// place an id is named the same.
fn renumber_llbc_ids(krate: &mut TranslatedCrate) {
    use charon_lib::{
        ast::BodyVisitable as _,
        llbc_ast::{BlockId, StatementId},
    };
    let mut blocks: blaze_hash::HashMap<BlockId, BlockId, std::alloc::Global> =
        blaze_hash::HashMap::default(); // alloc: an id map, once per crate
    let mut statements: blaze_hash::HashMap<StatementId, StatementId, std::alloc::Global> =
        blaze_hash::HashMap::default(); // alloc: an id map, once per crate
    for decl in krate.fun_decls.iter_mut() {
        decl.body.dyn_visit_in_body_mut(|id: &mut BlockId| {
            let next = BlockId::new(blocks.len());
            *id = *blocks.entry(*id).or_insert(next);
        });
        decl.body.dyn_visit_in_body_mut(|id: &mut StatementId| {
            let next = StatementId::new(statements.len());
            *id = *statements.entry(*id).or_insert(next);
        });
    }
}

/// A panic's message, when it has one.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned() // alloc: an error
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone() // clone: an error
    } else {
        "a panic without a message".to_owned() // alloc: an error
    }
}

/// `krate` as the `.llbc` JSON Charon's export writes (`CrateData` through `serde_json`), which
/// Aeneas reads. The same crate gives the same bytes.
pub fn llbc_json(krate: &TranslatedCrate) -> Vec<u8> {
    let data = CrateData {
        charon_version: CharonVersion(charon_lib::VERSION.to_owned()), // alloc: the version, once
        translated: krate.clone(), // clone: CrateData owns its crate; the caller keeps theirs
        has_errors: false,
    };
    // A `TranslatedCrate` has no map with non-string keys or float that JSON cannot hold, so
    // serializing it cannot fail; a failure is a bug in Charon's serializers, and fails this job.
    serde_json::to_vec(&data).expect("Charon's `CrateData` always serializes to JSON") // alloc: the file
}
