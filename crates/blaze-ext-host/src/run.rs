//! Running a workspace's extensions over one crate: their facts to a fixpoint, then their
//! lints over every body, each call a job of its own ([`crate::job`]).
//!
//! What comes back is what every lint of blaze gives, a [`Diag`], under the name
//! `<extension>::<lint>`, sorted by where it is, with the jobs that failed beside them: a
//! failed job costs its own findings and nothing else.

use std::fmt;

use base_db::{Db, FileId};
use blaze_diagnostics::model::{Diag, Level as DiagLevel, Span};
use blaze_ext::{
    Level,
    host::{self, Finding, HostKey, Limits, Lookup, NoFacts, Source},
    mir::MirSpan,
};
use hir_def::DefWithBodyId;

use crate::{
    facts::{self, Cache, Decl, Node, Plant, Runner, Slot, View},
    front::{self, Bodies},
    job::{self, JobError, What},
    load::Loaded,
    manifest::Declared,
};

/// One extension a crate is checked with.
pub struct Using<'l> {
    pub loaded: &'l Loaded,
    /// The levels the workspace sets, by lint name.
    pub levels: &'l [(String, Level)],
    /// Every job twice, compared ([`Declared::replay`]).
    pub replay: bool,
}

impl<'l> Using<'l> {
    /// `loaded` as the workspace declares it.
    pub fn of(loaded: &'l Loaded, declared: &'l Declared) -> Using<'l> {
        Using { loaded, levels: &declared.levels, replay: declared.replay }
    }
}

/// What the extensions found in a crate.
#[derive(Debug, Default)]
pub struct Checked {
    /// By file, then position, then lint: the same on every run.
    pub diagnostics: Vec<Diag>,
    /// The jobs that failed, in the order run. The check goes on past each.
    pub failed: Vec<JobError>,
    /// The cycles whose fact did not settle, each said by name.
    pub unsettled: Vec<String>,
    /// The facts computed and not found, each with why: what `blaze why` says of them.
    pub computed: Vec<String>,
    /// The functions no extension ran on, since their MIR could not be built.
    pub unbuilt: Vec<String>,
}

/// Why a crate could not be checked with its extensions at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    Unidentified(front::Unidentified),
    /// A finding is in a file the caller gives no name: it can't be said where it is, and is
    /// never left out.
    Unnamed {
        extension: String,
        lint: String,
        item: String,
    },
    /// A finding of a body with no place in the source at all.
    Unplaced {
        extension: String,
        lint: String,
        item: String,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::Unidentified(it) => it.fmt(f),
            CheckError::Unnamed { extension, lint, item } => write!(
                f,
                "extension `{extension}`: lint `{lint}` has a finding in `{item}`, in a file with no name to report it in"
            ),
            CheckError::Unplaced { extension, lint, item } => write!(
                f,
                "extension `{extension}`: lint `{lint}` has a finding in `{item}`, whose body is written nowhere in the source"
            ),
        }
    }
}

impl std::error::Error for CheckError {}

/// Names files as diagnostics show them.
// dyn: asked once per finding
pub type NameOf<'a> = &'a dyn Fn(FileId) -> Option<String>;

/// Checks `krate` with each of `using`, under `limits` per job, facts found in or added to
/// `cache`.
pub fn check(
    db: &Db,
    krate: hir::Crate,
    using: &[Using<'_>],
    limits: Limits,
    cache: &mut Cache,
    name_of: NameOf<'_>,
) -> Result<Checked, CheckError> {
    blaze_query::db::attach(db, || {
        let key = HostKey::of(db);
        let bodies = front::bodies(db, krate, &key).map_err(CheckError::Unidentified)?;
        let nodes: Vec<Node> = bodies
            .bodies
            .iter()
            .map(|it| Node {
                name: it.prefetched.name.clone(), // clone: a name per body, once per check
                which: it.which,
                identity: it.identity,
                callees: it.callees.clone(), // clone: as the name
            })
            .collect(); // alloc: the crate's items, once per check
        let mut checked = Checked { unbuilt: bodies.unbuilt.clone(), ..Checked::default() }; // clone: error path
        for it in using {
            one(db, &key, &bodies, &nodes, it, limits, cache, name_of, &mut checked)?;
        }
        checked.diagnostics.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        Ok(checked)
    })
}

/// What orders two diagnostics: where, then which lint, then what it says.
fn sort_key(diag: &Diag) -> (&str, u32, u32, &str, &str) {
    let span = diag.span.primary_span();
    (
        span.map_or("", |it| it.file.as_str()),
        span.map_or(0, |it| it.lo),
        span.map_or(0, |it| it.hi),
        diag.lint_name().unwrap_or(""),
        diag.message.as_str(),
    )
}

/// One extension over the crate.
fn one(
    db: &Db,
    key: &HostKey,
    bodies: &Bodies<'_>,
    nodes: &[Node],
    using: &Using<'_>,
    limits: Limits,
    cache: &mut Cache,
    name_of: NameOf<'_>,
    checked: &mut Checked,
) -> Result<(), CheckError> {
    let loaded = using.loaded;
    let extension = loaded.extension();
    let order = loaded.fact_order();
    // The facts in solve order: `decls[i]` is `extension.facts[order[i]]`.
    let position = |name: &&str| order.iter().position(|at| extension.facts[*at].name == *name);
    let names: Vec<&str> = order.iter().map(|at| extension.facts[*at].name).collect(); // alloc: a few names, once per extension per check
    let mut decls = Vec::with_capacity(order.len()); // alloc: as above
    for &at in order {
        let decl = &extension.facts[at];
        // `bottom` is the extension's code too: a job.
        let bottom = job::run(key, limits, &NoFacts, None, using.replay, |cx| {
            let mut out = cx.vec();
            (decl.bottom)(&mut out);
            out.to_vec() // alloc: the fact's bottom, kept for the solve
        });
        match bottom {
            Ok((bottom, _)) => decls.push(Decl {
                name: decl.name.to_owned(), // alloc: once per extension per check
                requires: decl
                    .requires
                    .iter()
                    .filter(|it| **it != decl.name)
                    .filter_map(|it| position(it))
                    .collect(),
                bottom,
            }),
            Err(failure) => {
                checked.failed.push(JobError {
                    extension: loaded.name().to_owned(),    // alloc: error path
                    what: What::Fact(decl.name.to_owned()), // alloc: error path
                    item: "its `bottom`".to_owned(),        // alloc: error path
                    failure,
                });
                // Without a fact's bottom nothing of the extension can be solved or asked.
                return Ok(());
            }
        }
    }
    let mut runner = FactRunner { key, bodies, using, names: &names, limits };
    let solved = facts::solve(loaded.key(), &decls, nodes, cache, &mut runner, Plant::None);
    checked.failed.extend(solved.failed.into_iter().map(|(_, _, error)| error));
    checked
        .unsettled
        .extend(solved.unsettled.iter().map(|it| it.said(loaded.name(), &decls, nodes)));
    checked.computed.extend(solved.computed.iter().map(|it| it.said(loaded.name(), &decls, nodes)));

    for (at, lint) in extension.lints.iter().enumerate() {
        let level = loaded.level(at, using.levels);
        if level == Level::Allow {
            continue;
        }
        let requires: Vec<usize> = lint.requires.iter().filter_map(|it| position(it)).collect(); // alloc: a few, once per lint per check
        let name = loaded.lint_name(at);
        for (node, body_of) in bodies.bodies.iter().enumerate() {
            let get = solved.table.for_lint(&requires, nodes, node as u32);
            let source = Facts { get: &get, names: &names, bodies };
            let ran = job::run(key, limits, &source, Some(at as u32), using.replay, |cx| {
                let body = host::body(key, body_of.item, body_of.mir, &body_of.prefetched);
                (lint.run)(cx, &body);
            });
            let findings = match ran {
                Ok(((), findings)) => findings,
                Err(failure) => {
                    checked.failed.push(JobError {
                        extension: loaded.name().to_owned(),    // alloc: error path
                        what: What::Lint(lint.name.to_owned()), // alloc: error path
                        item: body_of.prefetched.name.clone(),  // clone: error path
                        failure,
                    });
                    continue;
                }
            };
            for finding in findings {
                let placing = Placing { db, def: body_of.def, name_of };
                let diag =
                    placing.diag(&finding, &name, level, loaded.name()).map_err(|unnamed| {
                        let extension = loaded.name().to_owned(); // alloc: error path
                        let lint = lint.name.to_owned(); // alloc: error path
                        let item = body_of.prefetched.name.clone(); // clone: error path
                        match unnamed {
                            true => CheckError::Unnamed { extension, lint, item },
                            false => CheckError::Unplaced { extension, lint, item },
                        }
                    })?;
                checked.diagnostics.push(diag);
            }
        }
    }
    Ok(())
}

/// A job's facts: the table through what the job may read, by the names and items an
/// extension asks with.
struct Facts<'v, 't, 'db> {
    // dyn: asked once per `Cx::fact`
    get: &'v dyn Fn(usize, u32) -> Slot<'t>,
    /// The facts' names in solve order.
    names: &'v [&'v str],
    bodies: &'v Bodies<'db>,
}

impl<'v, 't: 'v> Source for Facts<'v, 't, '_> {
    fn fact(&self, name: &str, item: blaze_ext::Item) -> Lookup<'_> {
        let Some(fact) = self.names.iter().position(|it| *it == name) else {
            return Lookup::Undeclared;
        };
        let Some(node) = self.bodies.index_of(host::def_of(item)) else {
            return Lookup::Unknown;
        };
        match (self.get)(fact, node) {
            Slot::Value(bytes) => Lookup::Value(bytes),
            Slot::Failed => Lookup::Failed,
            Slot::Undeclared => Lookup::Undeclared,
        }
    }
}

/// The extension's fact functions, each call a job.
struct FactRunner<'r, 'db> {
    key: &'r HostKey,
    bodies: &'r Bodies<'db>,
    using: &'r Using<'r>,
    names: &'r [&'r str],
    limits: Limits,
}

impl Runner for FactRunner<'_, '_> {
    type Error = JobError;

    fn run(&mut self, view: &View<'_>) -> Result<Vec<u8>, JobError> {
        let (fact, node) = view.of();
        let loaded = self.using.loaded;
        let decl = &loaded.extension().facts[loaded.fact_order()[fact]];
        let body_of = &self.bodies.bodies[node as usize];
        let get = |fact: usize, of: u32| view.get(fact, of);
        let source = Facts { get: &get, names: self.names, bodies: self.bodies };
        let key = self.key;
        job::run(key, self.limits, &source, None, self.using.replay, |cx| {
            let body = host::body(key, body_of.item, body_of.mir, &body_of.prefetched);
            let mut out = cx.vec();
            (decl.run)(cx, &body, &mut out);
            out.to_vec() // alloc: the fact, kept by the solve
        })
        .map(|(bytes, _)| bytes)
        .map_err(|failure| JobError {
            extension: loaded.name().to_owned(),    // alloc: error path
            what: What::Fact(decl.name.to_owned()), // alloc: error path
            item: body_of.prefetched.name.clone(),  // clone: error path
            failure,
        })
    }
}

/// Where one body's findings go.
struct Placing<'p> {
    db: &'p Db,
    def: DefWithBodyId,
    name_of: NameOf<'p>,
}

impl Placing<'_> {
    /// The span of `at`, or of the body when `at` has no expression of its own. `Err(true)`:
    /// its file has no name; `Err(false)`: the body is written nowhere.
    fn span(&self, at: MirSpan) -> Result<(Span, bool), bool> {
        let (placed, exact) = match front::place(self.db, self.def, at) {
            Some(it) => (it, true),
            None => (front::whole(self.db, self.def).ok_or(false)?, false),
        };
        let file = (self.name_of)(placed.0).ok_or(true)?;
        Ok((Span::new(&file, placed.1), exact))
    }

    /// The diagnostic `finding` is, as the lint `name` at `level`.
    fn diag(
        &self,
        finding: &Finding,
        name: &str,
        level: Level,
        extension: &str,
    ) -> Result<Diag, bool> {
        let (span, exact) = self.span(finding.at)?;
        let mut diag = Diag::lint(name, finding.message.as_str(), span);
        if level == Level::Deny {
            diag.level = DiagLevel::Error;
        }
        if !exact {
            diag.note("no expression of the function's source reaches this finding; it is placed at the function");
        }
        for note in &finding.notes {
            diag.note(note.as_str());
        }
        for help in &finding.helps {
            diag.help(help.as_str());
        }
        for fix in &finding.fixes {
            // A fix with no place of its own can't be applied: it is said, not offered.
            match self.span(fix.at) {
                Ok((span, true)) => {
                    diag.suggestion(
                        span,
                        fix.message.as_str(),
                        fix.replacement.as_str(),
                        fix.applicability,
                    );
                }
                _ => {
                    diag.help(format!("{}: `{}`", fix.message, fix.replacement));
                }
            }
        }
        diag.help(format!(
            "reported by the extension `{extension}`; its level is set under \
             `[workspace.metadata.blaze.extensions.{extension}.lints]`"
        ));
        Ok(diag)
    }
}

/// The diagnostics of `checked`'s failed jobs and unsettled cycles: each an error at no
/// place, naming the extension, so a failed job is seen where findings are.
pub fn failures(checked: &Checked) -> Vec<Diag> {
    let failed = checked.failed.iter().map(JobError::to_string);
    failed
        .chain(checked.unsettled.iter().cloned()) // clone: error path
        .map(|message| Diag::new(DiagLevel::Error, None, message))
        .collect()
}
