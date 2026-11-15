//! [`Check`] over the ported front end: a body's steps run in a database whose every crate was
//! opened from its `Surface` record by [`crate::surface`], each step's trace taken from the item
//! store (`hir_ty::body_store`), which classifies every row of a step's read list through
//! `blaze_item_hash::reads::fact_of` and encodes the step's answer with no `Gid` and no
//! position. Reused, not redone: one classifier, one encoding.
//!
//! - **Steps.** Inference (`entries::run_infer`), the built MIR of each owner the body's
//!   lowering lists (`entries::places`, `entries::run_mir`), the borrow check's verdict
//!   (`hir_ty_borrowck::borrowck`, under the checker `hir_ty_borrowck::checker_name`). Each
//!   step's trace (`Installed::trace`) is `Kept { facts, scans, answer }` or `Refused(refusal)`
//!   (`scans` names the query of each whole-table read, told when its fact is refused); a
//!   refused one fails the task by `Refusal::name` (for a read `fact_of` refused, its query
//!   kind, `blaze_item_hash::reads::GENERIC_CONST_EVAL` and friends), and a step with no trace
//!   by `Untraced::what`. The verdict's answer is `gate::encode_verdict`'s bytes, a pass none:
//!   a body with a borrow error is a record too, its errors in its payload, read back with
//!   `gate::decode_verdict`.
//! - **Asks.** Each `FactRef` of a trace is classified ([`Front::row`]) and asked through
//!   [`Asks`]: the body's own earlier steps and its own code identity are covered by the record
//!   itself; a fact of an item is asked by its id ([`crate::facts::id_of`]), a fact another body
//!   makes included; a read of a `const fn`'s MIR (what an evaluation of it reads) is an ask of
//!   that function's made `ConstValue` fact, whose digest is its MIR's; any other read of
//!   another body's result is refused by the query's name (`OTHER_*`).
//! - **Made facts.** A const's or static's value, a variant's discriminant (lane items'
//!   `ConstValue`), a `const fn`'s built MIR (as an evaluation reads it), a function's hidden
//!   types (`HiddenTypes`), the escaped items' digest, the function's own effect summary.
//!
//! The body is found by the driver's item id through [`Front::find`], the command's: the
//! driver names an item by blaze-unitkey's id (`StableId::of` of its module path and kind),
//! which is not blaze-items' item key, so the lookup is by that id, never by a position.
//!
//! What stops a body of a crate opened from its record being checked here, as read in the tree
//! on 2026-11-14 (none of it is in this lane's files; each is listed for its owner):
//!
//! 1. The body's syntax is the whole file's, not the item's: `item_body_syntax`
//!    (crates/hir-def/src/expr_store/body.rs) reads `file_body_syntax`, which reads the file's
//!    span map, `ast_id_map` and root, so the whole file is parsed from its `FileText` input.
//!    The reader should serve the item's own green tree and its `ErasedFileAstId` anchor from
//!    the file image (blaze-unitkey's `FileImage` record) as an input of the item, so a body
//!    task needs nothing else of the file.
//! 2. Resolved: the item store's entries take a body by its `DefWithBodyId`, which
//!    [`Front::find`] gives from the driver's id; nothing a body task calls names a body by a
//!    position.
//! 3. Opening a crate from its pages is proportional to its modules: `pages::served`
//!    (crates/hir-def/src/nameres/frozen/pages.rs) builds a `ModulesMap` with one entry per
//!    module of the crate, and interns a `PagedScope` per module, at the first read of the
//!    crate in a process. Within the floor only if that table is the record's own sorted array
//!    read in place.
//! 4. Resolved: `hir_ty::body_store::install_trace` gives a store with no store of records to
//!    read, which runs every step, files nothing and keeps each trace for `Installed::trace`.
//! 5. Resolved: under a trace store `borrowck` offers its verdict whatever it is
//!    (`gate::encode_verdict`), and the old cache is not read (`check_cache::served_body` gives
//!    nothing while `body_store::tracing`).
//! 6. The effect summary is transitive: `walk::summary_in` (crates/hir-ty-pure/src/walk.rs)
//!    folds its callees' summaries into each instance's. The made fact needs the body's own
//!    part, cut at `summarize` before the callees are folded: `hir_ty_pure::own_effects`, not
//!    built yet; this file calls it as `own_effects(db: &Db, func: FunctionId) ->
//!    Result<OwnEffects, &'static str>` with `OwnEffects::digest(&self) -> ContentId`.
//! 7. The escaped items' digest, `blaze_item_hash::values::escaped(db: &Db, def:
//!    DefWithBodyId) -> Result<Option<ContentId>, &'static str>`, is not built yet.
//!
//! 8. A surface is refused (`crate::surface::IMAGE_UNASKED`, `hir_def::deps::Refused::Unasked`)
//!    when the crate's image depends on what no fact of a shape names: a procedural macro
//!    whose expansion traced reading something (a variable, a file: `ExpansionReads::Traced`,
//!    crates/hir-def/src/deps/serve.rs `unasked`), and a file outside the VFS the crate read
//!    (`Files::included_by`). Real crates reach both: a proc macro that reads an environment
//!    variable or a file, and an `include!` of `OUT_DIR`'s file where the build script's output
//!    is outside the VFS. A late-read fact for a traced expansion's names is the missing piece.
//! 9. A late read inside a body (`env!`, `option_env!`, `include_str!` in a function) is no
//!    `late_id` ask of the body's shape: the item store has no `FactRef` for it. Its rows reach
//!    the classifier as input rows (the process variable's `ProcessEnv` input through
//!    `base_db::process_var`, the crate's own variables through `Crate::env`, an included
//!    file's `FileText`), which `blaze_item_hash::reads::input` (crates/blaze-item-hash/src/reads.rs:301)
//!    refuses by the input kind's name, so the step's trace is `Refused` and the body task fails
//!    by that name (`Front::take`) and files nothing. Read, not run: whether every such row
//!    is an input row and none is covered was not checked. The fix is the classifier's: a
//!    `FactRef` of a late read (macro, name) that `crate::facts::id_of` maps to
//!    `record::late_id`, valued as the expansion saw it.
//!
//! Floor: the steps are the body's own check. Not yet at it beneath this file: (1) parses the
//! whole file, (3) reads per module at a crate's first open.

#![blaze::sans_io]

use std::alloc::{Allocator, Global};

use base_db::Db;
use blaze_ids::ContentId;
use blaze_item_hash::values;
use blaze_items::{ItemId, ItemKey, Resolved, key, kinds as items};
use blaze_keyed::{driver::TaskFailed, kind::Steps, record::StableId};
use hir_def::{DefWithBodyId, HasModule};
use hir_ty::{
    body_store::{Classified, Installed, Refusal, Step, Traced, entries, step_value},
    next_solver::goal_store::{FactObject, FactRef},
};

use super::{
    Check,
    exports::{Export, Exports},
    payload::Writer,
    reads::{Asks, Row},
};

/// The front end over a universe of opened surfaces.
pub struct Front<'u> {
    /// The snapshot every crate of the unit's universe was opened into from its `Surface`
    /// record (`crate::surface::Opened::give`). It keeps read lists (`blaze_memo::Reads::Kept`),
    /// else every trace is refused as `Refusal::Unshareable`.
    pub db: &'u Db,
    /// The item store of `db`, from `hir_ty::body_store::install_trace(db,
    /// Arc::new(blaze_item_hash::Bodies), build)`: the one classifier and encoder of a step's run.
    pub traces: &'u Installed,
    /// The body the driver's item id names in `db`: the command's lookup by blaze-unitkey's id.
    // dyn: one lookup per task, supplied by the command, which alone holds both id schemes.
    pub find: &'u (dyn Fn(StableId) -> Option<DefWithBodyId> + Sync),
    /// Where a refused task tells its lines (rule 19): the command keeps them under the unit
    /// (`Command::tell`), as `crate::surface::surface_telling`'s. Called on a refusal alone.
    // dyn: the command's sink, called only where a task fails
    pub told: &'u (dyn Fn(&str) + Sync),
}

/// What a body the opened surfaces do not have is refused as.
pub const NO_BODY: &str = "a body task: the driver's item id names no body of the opened surfaces";
/// What MIR owners that the lowering and the inference list differently are refused as.
pub const MIR_OWNERS: &str = "a body task: the body's MIR owners differ between its lowering and its inference";
/// What a body with no borrow checker set is refused as.
pub const NO_CHECKER: &str = "a body task: the build's borrow checker is off, so no verdict is a record";
/// A made fact that cannot be hashed now.
pub const UNHASHED_EXPORT: &str = "a body task: a fact the body makes that cannot be hashed now";

/// What a read of another body's result that is no made fact is refused as, by the query.
pub const OTHER_INFER: &str = "a body's read of another body's inference (`STEP_INFER`): no fact a body makes holds it";
pub const OTHER_MIR: &str =
    "a body's read of another body's MIR (`STEP_MIR`) that is not a `const fn`'s: no fact a body makes holds it";
pub const OTHER_LOWER: &str = "a body's read of another body's lowering (`STEP_LOWER`): no fact a body makes holds it";
pub const OTHER_MATCH: &str = "a body's read of another body's match check (`MATCH_CHECK`): no fact a body makes holds it";
pub const OTHER_CODE: &str = "a body's read of another item's code identity (`CODE`): read by code records only";
pub const OTHER_HASHES: &str = "a body's read of another item's hashes (`HASHES`): no fact a body makes holds it";
/// A read of a type alias's hidden type (`type_alias_impl_trait`, `#[define_opaque]`).
pub const ALIAS_HIDDEN: &str = "a body's read of a type alias's hidden type (`HIDDEN_TYPES` of an alias): its defining bodies make no fact of it yet";

/// The fact kinds that are a body's result, with what a read of another's is refused as.
fn body_result(kind: blaze_query::QueryKind) -> Option<&'static str> {
    use values::kinds as own;
    let kinds = [
        (own::STEP_INFER, OTHER_INFER),
        (own::STEP_MIR, OTHER_MIR),
        (own::STEP_LOWER, OTHER_LOWER),
        (own::MATCH_CHECK, OTHER_MATCH),
        (items::CODE, OTHER_CODE),
        (items::HASHES, OTHER_HASHES),
    ];
    kinds.iter().find(|(it, _)| *it == kind).map(|&(_, what)| what)
}

impl Front<'_> {
    /// Takes the trace of `step` of `def` under `schema`: its answer as a section, each of its
    /// facts classified and asked. `run` is the step's own query, asked again only where a
    /// read of it was refused ([`Front::tell_readers`]).
    fn take<A: Allocator>(
        &self,
        def: DefWithBodyId,
        step: Step,
        schema: blaze_format::SchemaHash,
        asks: &mut Asks<'_, A>,
        payload: &mut Writer,
        run: impl FnOnce(),
    ) -> Result<(), TaskFailed> {
        match self.traces.trace(def, step, schema).as_deref() {
            Err(untraced) => Err(TaskFailed::Refused { what: untraced.what() }),
            Ok(Traced::Refused(refusal)) => {
                if let Refusal::Read(_) = refusal {
                    self.tell_readers(def, step, run);
                }
                Err(TaskFailed::Refused { what: refusal.name() })
            }
            Ok(Traced::Kept { facts, scans, answer }) => {
                for &fact in facts.iter() {
                    let row = self.row(def, fact);
                    // A refused whole-table read names the query that made it (rule 19):
                    // `TaskFailed::RefusedFact` has no field for it, so the told line carries it.
                    if let Row::RefusedFact { what, kind, subject } = &row
                        && let Some((_, query)) = scans.iter().find(|(scan, _)| *scan == fact)
                    {
                        tracing::error!(
                            target: "blaze",
                            query = *query,
                            step = ?step,
                            rule = *what,
                            kind = *kind,
                            subject = ?subject,
                            "body task: the query `{query}` read a whole table, refused by `{what}`"
                        );
                    }
                    asks.offer(row)?;
                }
                payload.push(step as u8, schema.0, answer);
                Ok(())
            }
        }
    }

    /// What `fact`, a row of a step of `def`, stands for in the record.
    pub fn row(&self, def: DefWithBodyId, fact: FactRef) -> Row {
        let db = self.db;
        let own = values::body_key(db, def);
        let absolute = match fact.subject == values::OWN {
            true => FactRef { subject: own, ..fact },
            false => fact,
        };
        if let Some(what) = body_result(absolute.kind) {
            if absolute.subject == own {
                return Row::Covered;
            }
            // An evaluation's read of a `const fn`'s MIR: the function's made `ConstValue`.
            if absolute.kind == values::kinds::STEP_MIR && self.is_const_fn(&absolute.subject) {
                return Row::Fact(Export::ConstValue.id(&absolute.subject));
            }
            return refused(what, &absolute);
        }
        if absolute.kind == items::HIDDEN_TYPES
            && matches!(key::resolve(db, ItemKey(absolute.subject)), Some(Resolved::Item(ItemId::Alias(_))))
        {
            return refused(ALIAS_HIDDEN, &absolute);
        }
        match crate::facts::id_of(&absolute) {
            Ok(id) => Row::Fact(id),
            Err(error) => {
                Row::RefusedFact { what: error.what, kind: error.kind, subject: error.subject }
            }
        }
    }

}

/// `fact` refused by the rule `what`, naming the fact's kind and subject.
fn refused(what: &'static str, fact: &FactRef) -> Row {
    Row::RefusedFact { what, kind: fact.kind.0, subject: fact.subject }
}

impl Front<'_> {
    /// Tells one line per row of `step` of `def` the classifier refuses (rule 19): the body's
    /// item by kind and path, the row's kind, the rule, the innermost query that read it and
    /// its path of asks up to the step's work, and for a row of a file's text or root which
    /// file (`reads::file_of_row`). `run` is the step's work run again outside its query
    /// (`entries::rerun_unstored`) under `blaze_memo::record_readers`: the step's query is a
    /// fact boundary, so asking it again would list that one row and keep its run's rows behind
    /// it; the work's own reads are named by the work, the rest by the memo hits it asks, whose
    /// lists still hold what each run read. Only a failing task pays for it (one step run again).
    fn tell_readers(&self, def: DefWithBodyId, step: Step, run: impl FnOnce()) {
        use core::fmt::Write as _;
        let db = self.db;
        let item = ItemId::of_body(db, def);
        let Ok(((), readers)) = blaze_memo::record_readers(blaze_memo::Database::memo(db), run) else {
            let mut line = blaze_mem::String::new_in(Global);
            blaze_mem::text::format_into(
                &mut line,
                format_args!(
                    "{:?} `{}`: its {step:?} step read a row no fact stands for; its readers were not recorded (the snapshot keeps no read lists)",
                    item.kind(),
                    key::path(db, item),
                ),
            );
            (self.told)(line.as_str());
            return;
        };
        for (read, reader) in readers.rows.iter() {
            let Classified::Refused(rule) = blaze_item_hash::reads::fact_of(db, def, step, *read) else { continue };
            let mut line = blaze_mem::String::new_in(Global);
            _ = write!(
                line,
                "{:?} `{}`: its {step:?} step read `{}` ({rule}), read by {reader}",
                item.kind(),
                key::path(db, item),
                read.kind().name(),
            );
            if let Some(file) = blaze_item_hash::reads::file_of_row(db, *read) {
                _ = write!(line, " of `{file}`");
            }
            for kind in readers.chain(*reader).skip(1) {
                _ = write!(line, " <- {}", kind.name());
            }
            line.push_str(" <- (the step)");
            (self.told)(line.as_str());
        }
    }

    fn is_const_fn(&self, subject: &ContentId) -> bool {
        match crate::facts::body_of(self.db, subject) {
            Some(DefWithBodyId::FunctionId(func)) => hir_def::signatures::FunctionSignature::of(self.db, func).is_const(),
            _ => false,
        }
    }

    fn own_value(&self, def: DefWithBodyId, kind: blaze_query::QueryKind) -> Result<ContentId, TaskFailed> {
        let fact = FactRef { kind, subject: values::body_key(self.db, def), object: FactObject::None };
        values::value(self.db, def.krate(self.db), fact).ok_or(TaskFailed::Refused { what: UNHASHED_EXPORT })
    }
}

impl Check for Front<'_> {
    type Def = DefWithBodyId;

    fn def(&self, item: StableId) -> Result<DefWithBodyId, TaskFailed> {
        (self.find)(item).ok_or(TaskFailed::Refused { what: NO_BODY })
    }

    fn check<A: Allocator>(&self, def: DefWithBodyId, steps: Steps, asks: &mut Asks<'_, A>, payload: &mut Writer) -> Result<(), TaskFailed> {
        let db = self.db;
        let schema = entries::run_infer(db, def);
        self.take(def, Step::Infer, schema, asks, payload, || entries::rerun_unstored(db, def, Step::Infer, entries::Place::BODY))?;
        for place in entries::places(db, def) {
            let schema = entries::run_mir(db, def, place).map_err(|_| TaskFailed::Refused { what: MIR_OWNERS })?;
            self.take(def, Step::Mir, schema, asks, payload, || entries::rerun_unstored(db, def, Step::Mir, place))?;
        }
        // A body made for its MIR alone (a unit no `check` covers): no verdict, so its record
        // is never taken as checked (`Unserved::UNCHECKED`), and none of the borrow check's
        // reads is in its shape.
        if steps == Steps::Mir {
            return Ok(());
        }
        // `checker_name(db)` is `checker(mode(db))`, the name `borrowck` offers its verdict
        // under: the schema below must be the one the trace was kept at.
        let checker = hir_ty_borrowck::checker_name(db).ok_or(TaskFailed::Refused { what: NO_CHECKER })?;
        // Run the query itself, not `gate::checked`: its result is the trace's answer whatever
        // the verdict (`gate::encode_verdict`, a pass no bytes), so a failing body is a record
        // too, its errors in its payload; the gate's refusal is not this task's to raise.
        let _ = hir_ty_borrowck::borrowck(db, def);
        let schema = entries::borrowck_schema(db, def, checker);
        self.take(def, Step::Borrowck, schema, asks, payload, || _ = hir_ty_borrowck::borrowck(db, def))
    }

    fn exports(&self, def: DefWithBodyId, out: &mut Exports) -> Result<(), TaskFailed> {
        let db = self.db;
        let subject = values::body_key(db, def);
        match def {
            DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => {
                out.export(&subject, Export::ConstValue, self.own_value(def, items::CONST_VALUE)?);
            }
            DefWithBodyId::FunctionId(func) => {
                if hir_def::signatures::FunctionSignature::of(db, func).is_const() {
                    let mir = step_value(db, def, Step::Mir).ok_or(TaskFailed::Refused { what: UNHASHED_EXPORT })?;
                    out.export(&subject, Export::ConstValue, mir);
                }
                out.export(&subject, Export::HiddenType, self.own_value(def, items::HIDDEN_TYPES)?);
                let effects = hir_ty_pure::own_effects(db, func).map_err(|what| TaskFailed::Refused { what })?;
                out.export(&subject, Export::Effects, effects.digest());
            }
        }
        if let Some(escaped) = values::escaped(db, def).map_err(|what| TaskFailed::Refused { what })? {
            out.export(&subject, Export::Escaped, escaped);
        }
        Ok(())
    }
}
