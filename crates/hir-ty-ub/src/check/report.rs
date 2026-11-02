//! A finding's report, laid out as Miri's (`src/tools/miri/src/diagnostics.rs`,
//! `report_error` and `report_msg`): the error and where, the thread, the helps of its kind
//! (Tree Borrows' history of the tags involved, the earlier side of a race, where an allocation
//! was made and freed), and the backtrace. Built once, when the run has stopped.

use std::fmt::Write;

use base_db::{Db, FileId};
use either::Either;
use hir_def::{
    expr_store::{Body, ExpressionStore},
    signatures::FunctionSignature,
};
use hir_expand::InFile;
use hir_ty::{InferBodyId, mir::MirSpan};
use syntax::{SyntaxNodePtr, TextRange};

use super::{Checker, Frame};
use crate::{
    ids::{AllocId, SiteId},
    memory::MemError,
    ub::{Found, Ub},
};

pub(super) fn render<'db>(db: &'db Db, checker: &Checker<'db>, found: &Found, stack: &[Frame<'db>]) -> String {
    let r = Report { db, checker };
    // alloc: the edge: the report of a failure.
    let mut out = String::new();
    let _ = writeln!(out, "error: {}", found.ub);
    r.at(&mut out, found.at);
    let _ = writeln!(out, "   = note: on thread `{}`", checker.races.thread_display_name(found.thread));
    match &found.ub {
        Ub::Aliasing(e) => {
            for event in e.history() {
                match event.site {
                    Some(site) => {
                        let _ = writeln!(out, "help: {}", event.note);
                        r.at(&mut out, site);
                    }
                    None => {
                        let _ = writeln!(out, "   = help: {}", event.note);
                    }
                }
            }
        }
        Ub::Race(e) => {
            for help in e.help() {
                let _ = writeln!(out, "   = help: {help}");
            }
            if let Some((earlier, _)) = e.sites() {
                let _ = writeln!(out, "help: and (1) occurred earlier here");
                r.at(&mut out, earlier);
            }
        }
        // Miri: diagnostics.rs, `report_error`'s `PointerUseAfterFree` and `PointerOutOfBounds`
        // helps.
        Ub::Memory(MemError::UseAfterFree { alloc, .. } | MemError::OutOfBounds { alloc, .. }) => {
            r.alloc_history(&mut out, *alloc);
        }
        Ub::Leak(leaks) => {
            for leak in &leaks.leaked {
                let _ = writeln!(out, "help: {:?} was allocated here:", leak.alloc);
                r.at(&mut out, site_of(&r.checker.made_at, leak.alloc));
            }
        }
        _ => {}
    }
    if !stack.is_empty() {
        let _ = writeln!(out, "   = note: BACKTRACE on thread `{}`:", checker.races.thread_display_name(found.thread));
        for &(func, span, owner) in stack {
            let name = match func {
                // alloc: the edge: the report of a failure.
                Either::Left(func) => FunctionSignature::of(db, func).name.as_str().to_owned(),
                // alloc: the edge: the report of a failure.
                Either::Right(_) => "closure".to_owned(),
            };
            match r.place(owner, span) {
                Some(place) => {
                    let _ = writeln!(out, "   = note: inside `{name}` at {place}");
                }
                None => {
                    let _ = writeln!(out, "   = note: inside `{name}`");
                }
            }
        }
    }
    out
}

fn site_of(column: &[SiteId], alloc: AllocId) -> SiteId {
    column.get(alloc.index()).copied().unwrap_or(SiteId::START)
}

struct Report<'a, 'db> {
    db: &'db Db,
    checker: &'a Checker<'db>,
}

impl<'db> Report<'_, 'db> {
    fn alloc_history(&self, out: &mut String, alloc: AllocId) {
        let made = site_of(&self.checker.made_at, alloc);
        if made != SiteId::START {
            let _ = writeln!(out, "help: {alloc:?} was allocated here:");
            self.at(out, made);
        }
        let freed = site_of(&self.checker.freed_at, alloc);
        if freed != SiteId::START {
            let _ = writeln!(out, "help: {alloc:?} was deallocated here:");
            self.at(out, freed);
        }
    }

    /// `  --> path:line:column` of a site, when it has a place in the source.
    fn at(&self, out: &mut String, site: SiteId) {
        if let Some(site) = self.checker.sites.get(site)
            && let Some(place) = self.place(site.owner, site.span)
        {
            let _ = writeln!(out, "  --> {place}");
        }
    }

    /// `path:line:column`, 1-based, of the start of `span` in the body of `owner`.
    fn place(&self, owner: InferBodyId<'db>, span: MirSpan) -> Option<String> {
        let db = self.db;
        let (file, range) = source_range(db, owner, span)?;
        let text: &str = db.file_text(file).text(db);
        let start = (u32::from(range.start()) as usize).min(text.len());
        let before = &text[..start];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        let root = db.source_root(db.file_source_root(file).source_root_id(db));
        Some(match root.source_root_ref(db).path_for_file(&file) {
            // alloc: the edge: the report of a failure.
            Some(path) => format!("{path}:{line}:{column}"),
            // alloc: the edge: the report of a failure.
            None => format!("{file:?}:{line}:{column}"),
        })
    }
}

/// The file and range of `span` in the body of `def` (as hir-ty-mir's `mir_pretty::errors`).
fn source_range(db: &Db, def: InferBodyId<'_>, span: MirSpan) -> Option<(FileId, TextRange)> {
    let (source_map, self_param_syntax) = match def {
        InferBodyId::DefWithBodyId(def) => {
            let body = &Body::with_source_map(db, def).1;
            (&**body, body.self_param_syntax())
        }
        InferBodyId::AnonConstId(def) => (ExpressionStore::with_source_map(db, def.loc(db).owner).1, None),
    };
    let span: InFile<SyntaxNodePtr> = match span {
        MirSpan::ExprId(e) | MirSpan::ExprEnd(e) => source_map.expr_syntax(e).ok()?.map(|it| it.into()),
        MirSpan::PatId(p) => source_map.pat_syntax(p).ok()?.map(|it| it.syntax_node_ptr()),
        MirSpan::BindingId(b) => source_map
            .patterns_for_binding(b)
            .iter()
            .find_map(|p| source_map.pat_syntax(*p).ok())?
            .map(|it| it.syntax_node_ptr()),
        MirSpan::SelfParam => self_param_syntax?.map(|it| it.syntax_node_ptr()),
        // The interpreter runs no optimized body, so no span of one reaches it.
        MirSpan::Unknown | MirSpan::Inlined(_) => return None,
    };
    Some((span.file_id.original_file(db).file_id(db), span.value.text_range()))
}
