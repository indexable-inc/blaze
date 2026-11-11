//! `program_lifetime` (in `blaze::edition`, `deny` where it fires): the check of
//! `#[blaze::program_lifetime]` (docs/v2/plan/design/facts.md "ProgramLifetime"). A type so
//! declared says its instances live until the program exits; this refuses the declaration
//! where that cannot hold:
//!
//! - its shape (`hir_ty_pure::program_lifetime::shape`): it has a parameter or a `Drop`, a
//!   field holds a value with a `Drop` or borrows for less than `'static`, or a field's type
//!   is one the check can't see into. The finding names the field path and the rule;
//! - it is another crate's to name (`pub` and exported) and its crate has no
//!   `#![blaze::program_lifetime]` at its root: other crates would build it wherever they
//!   like, and no check of this crate would see where;
//! - a record literal of it in the crate is not the argument of
//!   `blaze_mem::ProgramArena::alloc`, of a `Box::leak(Box::new(..))`, or in the initializer
//!   of a `static` ([`super::could_be_program_lifetime::constructions`]).
//!
//! Narrowed: only record literals are followed as constructions (a tuple struct's or a unit
//! variant's constructor is a path or a call, not followed yet), so a `program_lifetime`
//! tuple struct is checked for shape and visibility alone. No pass changes code on this fact
//! (`Facts::spendable`), so what the narrowing misses can cost a wrong lint, never wrong code.
//! No fix: every refusal is a decision about the type.

use hir_def::{AdtId, AttrDefId, attrs::AttrFlags, nameres::crate_def_map};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint,
    could_be_program_lifetime::{Kept, constructions, place_of},
};
use crate::{check::Cx, diag};

const LINT: Lint = Lint::ProgramLifetime;

/// Whether `krate` lets other crates name its program-lifetime types:
/// `#![blaze::program_lifetime]` at its root.
pub(super) fn opted_in(db: &base_db::Db, krate: base_db::Crate) -> bool {
    let root = crate_def_map(db, krate).root_module_id();
    AttrFlags::query(db, AttrDefId::ModuleId(root)).contains(AttrFlags::BLAZE_PROGRAM_LIFETIME)
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(adt.syntax()) {
        return;
    }
    let Some(def) = cx.adt_def(adt) else { return };
    let db = cx.db();
    let id = AdtId::from(def);
    if !hir_ty_pure::program_lifetime::declared(db, id) {
        return;
    }
    let name = hir_ty_pure::program_lifetime::adt_name(db, id);
    let krate = cx.krate().base();
    // alloc: a finding's notes, a few
    let mut notes = Vec::new();
    let message = if let Some(refused) = hir_ty_pure::program_lifetime::shape(db, id) {
        refused.said()
    } else if cx.is_exported(adt.syntax()) && !opted_in(db, krate) {
        format!(
            "`{name}` is `#[blaze::program_lifetime]`, but another crate can name it and build it anywhere: make it private, or put `#![blaze::program_lifetime]` at the crate's root to say its public types may be"
        )
    } else {
        let outside: Vec<_> = constructions(db, krate)
            .iter()
            .filter(|it| it.adt == id && it.into == Kept::Other)
            .collect();
        if outside.is_empty() {
            return;
        }
        for site in outside.iter().take(4) {
            if let Some((file, range)) = place_of(db, site) {
                notes.push((diag::of_file(file, range), "built here, outside the program arena"));
            }
        }
        format!(
            "`{name}` is `#[blaze::program_lifetime]`, but {} of its values {} built outside the program arena: build each as the argument of `ProgramArena::alloc`",
            outside.len(),
            if outside.len() == 1 { "is" } else { "are" }
        )
    };
    let node = adt.name().map_or(adt.syntax(), |it| it.syntax());
    let Some(range) = cx.range(node) else { return };
    let span = cx.span(range);
    let diagnostic = cx.blaze(LINT, adt.syntax(), span, message);
    for (at, note) in notes {
        diagnostic.span_note(at, note);
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}
