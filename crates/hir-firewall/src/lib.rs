//! An edit of a crate's body recomputes nothing of the crates that depend on it
//! (docs/v2/plan/design/item-hash.md, docs/v2/plan/design/front-end-on-memo.md): edits one
//! function body of crate A at a time and checks that, after an edit that changes no fact A
//! exports, no query homed in a dependent crate B runs again, and that the code compiled again
//! is the edited function's.
//!
//! In the memo an edit drops A's arena and every arena that borrows from it; B's entries come
//! back from B's records without running wherever the facts they read (their digests) are
//! unchanged. So the check reads the run log ([`blaze_memo::testing::RunLog`]): each run is
//! homed in an arena, a run in A's arena is A's own rerun (counted, [`Report::reran_own`]),
//! and a run in the arena of a crate that depends on A is a [`Leak`]: a fact B read moved, so
//! the edit changed what A exports. A run in any other arena (a dependency of A, the session)
//! is work first asked by the edit's text; those arenas are carried, so nothing there runs
//! again.
//!
//! [`Harness::new`] lists the bodies of A and of every crate that depends on it, asks for
//! everything a build asks of them (each body's check and code hash, each file module's item
//! checks, and with [`Codegen`] every instance codegen reaches from A), then asks again with
//! nothing changed (a commit that moves nothing makes no snapshot, so that is the same
//! snapshot, as for a build that changed nothing): what runs then is reported as
//! [`Report::volatile`]. Each [`Harness::edit`] inserts [`EDIT`] after a body's `{`, asks
//! again with the edited body's requests first, undoes the edit and asks again; each pass's
//! runs and code are judged ([`Leak`], [`CodeLeak`]).
//!
//! Every id (a crate, a body, a module, an instance) dies at the commit that drops its arena,
//! so the harness keeps nothing of one snapshot past a commit but stable names: the crate by
//! its [`CrateKey`], each body by its [`BodyKey`], each instance by its identity
//! ([`blaze_codegen::backend::Described::identity`]). After each commit it lists the bodies
//! and modules again.

#![forbid(unsafe_code)]
#![feature(allocator_api, btreemap_alloc)]

mod codegen;

use std::alloc::{Allocator, Global};
use std::fmt;

use base_db::{Crate, CrateKey, Db, FileId};
use blaze_ids::ContentId;
use blaze_memo::testing::{Ran, RunLog};
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, Lookup, ModuleDefId,
    nameres::crate_def_map,
    signatures::{ConstSignature, FunctionSignature, StaticSignature},
    src::HasSource,
};
use syntax::TextSize;

pub use codegen::Codegen;

/// What an edit inserts after a function body's `{`: a statement that changes the body's MIR
/// and code hash and none of the facts other items read of a body (no type an opaque type
/// hides, no binding held across an `.await`, no value of a `const fn`), and that needs no
/// item of any crate.
pub const EDIT: &str = " let _firewall_edit: u32 = 0;";

/// A body of A or of a crate that depends on it: a function's, a constant's or a static's, as
/// one snapshot has it.
struct Body {
    /// Live only in the snapshot it was listed in.
    def: DefWithBodyId,
    name: String,
    key: BodyKey,
    /// Whether it is A's: only A's bodies are edited.
    own: bool,
    /// Where a function of A has its body written in a file; `None` for a constant, a static,
    /// a function a macro wrote and every body of a dependent crate.
    site: Option<Site>,
}

/// A body by what stays across commits: its crate, its name (`_` for an unnamed constant), and
/// which of its crate's bodies of that name it is in the def map's order.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct BodyKey {
    krate: CrateKey,
    name: String,
    nth: u32,
}

#[derive(Clone, Copy)]
struct Site {
    vfs: FileId,
    /// Just after its body's `{`.
    at: TextSize,
    is_const: bool,
}

/// What a pass asked, for a leak to name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Request {
    Module(u32),
    Check(u32),
    Code(u32),
    Codegen,
}

/// What one pass of the requests ran and gave.
#[derive(Default)]
struct Pass {
    runs: Vec<(Ran, Request), Global>,
    /// Each body's code hash, in the order of the list the pass asked; `None` where it has
    /// none.
    codes: Vec<(BodyKey, Option<ContentId>), Global>,
    instances: Option<codegen::Keys>,
    /// Entries served from their records without running (this thread's counts).
    served: u64,
}

/// A query of a crate that depends on the edited one that ran again after the edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leak {
    /// The edited function.
    pub edited: String,
    /// In the pass after the edit was undone.
    pub undo: bool,
    /// The query, by its full path.
    pub query: String,
    /// The key, as the run log wrote it.
    pub key: String,
    /// What the pass asked when it ran.
    pub request: String,
}

/// Code compiled again that is not the edited function's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeLeak {
    /// Another body's code hash changed.
    Body { edited: String, undo: bool, body: String },
    /// An instance not of the edited body changed, appeared or went.
    Instance { edited: String, undo: bool, instance: String, how: &'static str },
    /// The edited body's code hash did not change: the edit is not seen by codegen.
    Unchanged { edited: String },
    /// After the undo, a body's code hash is not what it was before the edit.
    NotRestored { edited: String, body: String },
}

/// What the edits found.
#[derive(Debug, Default)]
pub struct Report {
    pub edits: usize,
    /// Over every pass after an edit or an undo: the queries that ran, wherever homed.
    pub executed: usize,
    /// Of those, the ones homed in the edited crate's arena.
    pub reran_own: usize,
    /// Over the same passes: entries served from their records without running.
    pub served: u64,
    pub leaks: Vec<Leak>,
    pub code: Vec<CodeLeak>,
    /// Queries that ran again in a snapshot that changed nothing.
    pub volatile: Vec<&'static str>,
    /// A's bodies no edit was made in: constants, statics, functions a macro wrote.
    pub unedited: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.leaks.is_empty() && self.code.is_empty() && self.volatile.is_empty()
    }

    /// The queries that leaked, each once, in order.
    pub fn leaked_queries(&self) -> Vec<&str> {
        let mut queries = Vec::new_in(Global);
        queries.extend(self.leaks.iter().map(|it| it.query.as_str()));
        queries.sort_unstable();
        queries.dedup();
        queries
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} edits, {} leaks, {} code leaks, {} volatile; {} runs ({} of the edited crate), \
             {} served from records",
            self.edits,
            self.leaks.len(),
            self.code.len(),
            self.volatile.len(),
            self.executed,
            self.reran_own,
            self.served
        )?;
        for query in &self.volatile {
            writeln!(f, "  volatile: {query}")?;
        }
        for leak in &self.leaks {
            let pass = if leak.undo { "undo of" } else { "edit of" };
            writeln!(
                f,
                "  leak: {pass} `{}` ran {} {} (asked by {})",
                leak.edited, leak.query, leak.key, leak.request
            )?;
        }
        for leak in &self.code {
            writeln!(f, "  code: {leak:?}")?;
        }
        Ok(())
    }
}

/// The harness over one crate of a database and the crates that depend on it. See the module.
pub struct Harness {
    krate: CrateKey,
    /// What the current snapshot has of A and its dependents, listed again after each commit.
    listed: Listed,
    codegen: Option<Codegen>,
    /// The queries that ran again with nothing changed.
    volatile: Vec<&'static str, Global>,
    /// The cold pass's code hashes: what each undo must give again.
    base: Pass,
}

/// What one snapshot has of A and of the crates that depend on it. Its ids are live only in
/// that snapshot.
struct Listed {
    krate: Crate,
    /// A's arena index.
    own: u32,
    /// The arena indices of the crates that depend on A, directly or not.
    dependents: Vec<u32, Global>,
    bodies: Vec<Body, Global>,
    modules: Vec<hir::Module, Global>,
}

impl Listed {
    /// The bodies and modules of `key`'s crate and of every crate that depends on it, as
    /// `db`'s snapshot has them.
    fn of(db: &Db, key: CrateKey) -> Result<Listed, String> {
        let krate = db.krate(key);
        let all = base_db::all_crates(db);
        let own = arena_index(&all, krate)?;
        let mut dependents = Vec::new_in(Global);
        let mut bodies = Vec::new_in(Global);
        let mut modules = Vec::new_in(Global);
        collect(db, krate, true, &mut bodies);
        modules.extend(hir::check::file_modules(db, hir::Crate::from(krate)));
        let mut reached = Vec::new_in(Global);
        krate.transitive_rev_deps(db, &mut reached);
        for dependent in reached {
            if dependent == krate {
                continue;
            }
            dependents.push(arena_index(&all, dependent)?);
            collect(db, dependent, false, &mut bodies);
            modules.extend(hir::check::file_modules(db, hir::Crate::from(dependent)));
        }
        Ok(Listed { krate, own, dependents, bodies, modules })
    }

    /// The place of the body `key` names in this list.
    fn position(&self, key: &BodyKey) -> Result<usize, String> {
        self.bodies
            .iter()
            .position(|it| it.key == *key)
            .ok_or_else(|| format!("no body is `{}` (#{} of its name) after the commit", key.name, key.nth))
    }
}

/// What the harness refuses of a database or its crate graph, typed: its message is made only
/// where it reaches the harness's own errors, which carry text.
#[derive(Debug, Clone, Copy)]
enum Refused {
    /// The database keeps no run log ([`log`]).
    NoRunLog,
    /// The crate is not in the crate graph ([`arena_index`]).
    NotInGraph(Crate),
    /// The crate's place in the crate graph is past `u32::MAX` ([`arena_index`]).
    PastU32(Crate),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NoRunLog => {
                f.write_str("the database keeps no run log: make it with `Db::logged` or `Db::for_tests`")
            }
            Refused::NotInGraph(krate) => write!(f, "{krate:?} is not in the crate graph"),
            Refused::PastU32(krate) => write!(f, "{krate:?} is past `u32::MAX` crates"),
        }
    }
}

impl From<Refused> for String {
    fn from(it: Refused) -> String {
        it.to_string()
    }
}

/// The run log of `db`: a harness reads what each request ran from it.
fn log(db: &Db) -> Result<&RunLog, Refused> {
    db.runs().ok_or(Refused::NoRunLog)
}

/// Writes `text` as `file`'s, in one edit and its commit: every id of the snapshot before is
/// dead after it.
fn write(db: &mut Db, file: FileId, text: &str) {
    let mut edit = db.edit();
    edit.set_file_text(file, text);
    _ = edit.commit();
}

/// The arena of `krate`: one past its place in the crate graph's topological order, as the
/// driver numbers arenas ([`blaze_memo::Builder::arena`]; 0 is the session's).
fn arena_index(all: &[Crate], krate: Crate) -> Result<u32, Refused> {
    let at = all.iter().position(|&it| it == krate).ok_or(Refused::NotInGraph(krate))?;
    u32::try_from(at + 1).map_err(|_| Refused::PastU32(krate))
}

impl Harness {
    /// Lists the bodies of `krate` (of `db`'s current snapshot) and of every crate that depends
    /// on it, asks for everything once, then once more with nothing changed. `db` must keep a
    /// run log ([`Db::logged`], [`Db::for_tests`]).
    pub fn new(db: &mut Db, krate: Crate, codegen: Option<Codegen>) -> Result<Harness, String> {
        log(db)?;
        let key = krate.key(db);
        let mut harness = Harness {
            krate: key,
            listed: Listed::of(db, key)?,
            codegen,
            volatile: Vec::new_in(Global),
            base: Pass::default(),
        };
        let cold = harness.pass(db, None)?;
        // No commit: one that moves nothing makes no snapshot, so asking again in this one is
        // what a build that changed nothing asks.
        let warm = harness.pass(db, None)?;
        harness.volatile.extend(warm.runs.iter().map(|(ran, _)| ran.query));
        harness.volatile.sort_unstable();
        harness.volatile.dedup();
        harness.base = cold;
        Ok(harness)
    }

    /// Asks for everything once more, as each pass does, and gives how many queries ran: what
    /// a bench times one pass by.
    pub fn ask_all(&self, db: &Db) -> Result<usize, String> {
        Ok(self.pass(db, None)?.runs.len())
    }

    /// The number of bodies listed, the dependents' included.
    pub fn body_count(&self) -> usize {
        self.listed.bodies.len()
    }

    /// The functions of A an edit can be made in, by their place and name. A place is of the
    /// current snapshot's list: one an edit leaves the bodies as they were keeps it.
    pub fn functions(&self) -> impl Iterator<Item = (usize, &str)> {
        self.listed
            .bodies
            .iter()
            .enumerate()
            .filter(|(_, it)| it.site.is_some())
            .map(|(at, it)| (at, it.name.as_str()))
    }

    /// The place of the function of A named `name` an edit can be made in.
    pub fn function(&self, name: &str) -> Option<usize> {
        self.functions().find(|&(_, it)| it == name).map(|(at, _)| at)
    }

    /// Every `stride`th function of A edited in turn with [`EDIT`], from the first.
    pub fn run(&mut self, db: &mut Db, stride: usize) -> Result<Report, String> {
        let mut report = Report::default();
        self.report_setup(&mut report);
        // By key: each edit's commits list the bodies again.
        let mut functions = Vec::new_in(Global);
        functions.extend(
            self.functions()
                .map(|(at, _)| self.listed.bodies[at].key.clone())
                .step_by(stride.max(1)),
        );
        for key in &functions {
            let at = self.listed.position(key)?;
            self.edit(db, at, EDIT, &mut report)?;
        }
        Ok(report)
    }

    /// What the setup found: the volatile queries and A's bodies no edit is made in.
    pub fn report_setup(&self, report: &mut Report) {
        report.volatile.clear();
        report.volatile.extend(self.volatile.iter().copied());
        report.unedited.clear();
        report.unedited.extend(
            (self.listed.bodies.iter())
                .filter(|it| it.own && it.site.is_none())
                .map(|it| it.name.clone()),
        );
    }

    /// Inserts `text` after the `{` of function `at`'s body (a place of [`Self::functions`]),
    /// asks again, undoes it and asks again, and reports what each pass ran of the dependent
    /// crates and compiled again.
    pub fn edit(
        &mut self,
        db: &mut Db,
        at: usize,
        text: &str,
        report: &mut Report,
    ) -> Result<(), String> {
        let body = &self.listed.bodies[at];
        let site =
            body.site.ok_or_else(|| format!("`{}` has no body written in a file", body.name))?;
        let before = db.file_text(site.vfs).text(db).as_str().to_owned();
        let offset = usize::from(site.at);
        let mut edited_text = String::with_capacity(before.len() + text.len());
        edited_text.push_str(&before[..offset]);
        edited_text.push_str(text);
        edited_text.push_str(&before[offset..]);
        let edited = body.key.clone();
        let name = body.name.clone();

        // The snapshot before the edit, asked with the edited body first: every answer is a hit
        // (the last pass asked the same of it), and each instance is told whether it is the
        // edited body's while its ids are live. Its runs are no edit's.
        let before_edit = self.pass(db, Some(at))?;

        write(db, site.vfs, &edited_text);
        self.listed = Listed::of(db, self.krate)?;
        let at = self.listed.position(&edited)?;
        let after_edit = self.pass(db, Some(at))?;
        self.judge(db, &name, &after_edit, false, report);
        self.judge_code(&name, &edited, &before_edit, &after_edit, false, site.is_const, report);

        write(db, site.vfs, &before);
        self.listed = Listed::of(db, self.krate)?;
        let at = self.listed.position(&edited)?;
        let after_undo = self.pass(db, Some(at))?;
        self.judge(db, &name, &after_undo, true, report);
        self.judge_code(&name, &edited, &after_edit, &after_undo, true, site.is_const, report);
        // By key: a body the undo did not give back, or gave another code hash, is not restored.
        let base = blaze_hash::HashMap::<&BodyKey, Option<ContentId>, Global>::from_iter_in(
            self.base.codes.iter().map(|(key, code)| (key, *code)),
            Global,
        );
        let mut restored = blaze_hash::HashSet::<&BodyKey, Global>::new_in(Global);
        for (key, is) in &after_undo.codes {
            restored.insert(key);
            if base.get(key) != Some(is) {
                report
                    .code
                    .push(CodeLeak::NotRestored { edited: name.clone(), body: key.name.clone() });
            }
        }
        for (key, _) in &self.base.codes {
            if !restored.contains(key) {
                report
                    .code
                    .push(CodeLeak::NotRestored { edited: name.clone(), body: key.name.clone() });
            }
        }

        report.edits += 1;
        report.executed += after_edit.runs.len() + after_undo.runs.len();
        report.served += after_edit.served + after_undo.served;
        Ok(())
    }

    /// Everything a build asks of A and its dependents, each request's runs taken from the log:
    /// with `first`, that body's requests first, then the modules' and the other bodies'.
    /// `db` must be in the snapshot [`Self::listed`] was listed in; with `first`, each instance
    /// is told whether it is that body's.
    fn pass(&self, db: &Db, first: Option<usize>) -> Result<Pass, String> {
        let log = log(db)?;
        let listed = &self.listed;
        let mut pass = Pass::default();
        pass.codes.extend(listed.bodies.iter().map(|it| (it.key.clone(), None)));
        // What ran before this pass (the setup's own listing) is no request's.
        _ = log.take();
        _ = blaze_memo::take_counts();
        if let Some(first) = first {
            self.ask_body(db, log, first, &mut pass);
        }
        for (at, module) in listed.modules.iter().enumerate() {
            _ = hir::check::check_module(db, *module, false, &|_: hir::DefWithBody| false);
            take(log, Request::Module(at as u32), &mut pass);
        }
        for at in 0..listed.bodies.len() {
            if Some(at) != first {
                self.ask_body(db, log, at, &mut pass);
            }
        }
        if let Some(codegen) = &self.codegen {
            let edited = first.map(|at| listed.bodies[at].def);
            let instances = codegen::instances(db, listed.krate, codegen, edited);
            take(log, Request::Codegen, &mut pass);
            pass.instances = Some(instances?);
        }
        pass.served = blaze_memo::take_counts().image_served;
        Ok(pass)
    }

    fn ask_body(&self, db: &Db, log: &RunLog, at: usize, pass: &mut Pass) {
        let def = self.listed.bodies[at].def;
        let index = at as u32;
        _ = hir::check::check_body(db, def, false);
        take(log, Request::Check(index), pass);
        pass.codes[at].1 = hir_ty_share::code::of_item(db, def).as_ref().ok().copied();
        take(log, Request::Code(index), pass);
    }

    /// Counts each run of `pass` homed in A's arena and reports each homed in a dependent's.
    /// `pass` is of the current list's snapshot.
    /// `db` is the snapshot `pass` ran in: a leak's key is written inside its `attach`.
    fn judge(&self, db: &Db, edited: &str, pass: &Pass, undo: bool, report: &mut Report) {
        for (ran, request) in &pass.runs {
            let home = ran.arena().index();
            if home == self.listed.own {
                report.reran_own += 1;
            } else if self.listed.dependents.contains(&home) {
                report.leaks.push(Leak {
                    edited: edited.to_owned(),
                    undo,
                    query: ran.query.to_owned(),
                    key: blaze_memo::attach(db, || ran.key()),
                    request: self.request_name(*request),
                });
            }
        }
    }

    /// Reports each body whose code hash changed from `before` to `after` but the edited one,
    /// the edited one's when it did not change, and each instance whose key changed that is
    /// not the edited body's ([`hir_ty_codegen::v2::HirInstance::of_body`], as each pass told
    /// it while its ids were live). A `const fn`'s callers in constants are compiled from its
    /// value, so they are not judged for one. Bodies are matched by key; `after` is of the
    /// current list's snapshot, and a body new in it had no code before.
    #[allow(clippy::too_many_arguments)]
    fn judge_code(
        &self,
        name: &str,
        edited: &BodyKey,
        before: &Pass,
        after: &Pass,
        undo: bool,
        is_const: bool,
        report: &mut Report,
    ) {
        let was_of = blaze_hash::HashMap::<&BodyKey, Option<ContentId>, Global>::from_iter_in(
            before.codes.iter().map(|(key, code)| (key, *code)),
            Global,
        );
        for (at, (key, is)) in after.codes.iter().enumerate() {
            let was = was_of.get(key).copied().flatten();
            if key == edited {
                if was.is_some() && was == *is {
                    report.code.push(CodeLeak::Unchanged { edited: name.to_owned() });
                }
            } else if was != *is
                && !(is_const
                    && !matches!(self.listed.bodies[at].def, DefWithBodyId::FunctionId(_)))
            {
                report.code.push(CodeLeak::Body {
                    edited: name.to_owned(),
                    undo,
                    body: key.name.clone(),
                });
            }
        }
        let (Some(was), Some(is)) = (&before.instances, &after.instances) else { return };
        let mut push = |instance: &codegen::Instance, how| {
            report.code.push(CodeLeak::Instance {
                edited: name.to_owned(),
                undo,
                instance: instance.name.clone(),
                how,
            });
        };
        // In identity order, so a report is the same from run to run.
        for (identity, then) in was {
            match is.get(identity) {
                Some(now) if now.key == then.key => {}
                Some(now) if !(then.of_edited || now.of_edited) => push(now, "changed"),
                Some(_) => {}
                None if !then.of_edited => push(then, "went"),
                None => {}
            }
        }
        for (identity, now) in is {
            if !was.contains_key(identity) && !now.of_edited {
                push(now, "appeared");
            }
        }
    }

    fn request_name(&self, request: Request) -> String {
        let listed = &self.listed;
        match request {
            Request::Module(at) => {
                format!("the item checks of module {:?}", listed.modules[at as usize])
            }
            Request::Check(at) => format!("the check of `{}`", listed.bodies[at as usize].name),
            Request::Code(at) => {
                format!("the code hash of `{}`", listed.bodies[at as usize].name)
            }
            Request::Codegen => "codegen's walk".to_owned(),
        }
    }
}

/// Moves the runs `log` logged since the last request into `pass`, as `request`'s.
fn take(log: &RunLog, request: Request, pass: &mut Pass) {
    pass.runs.extend(log.take().into_iter().map(|ran| (ran, request)));
}

/// Adds every body of `krate` declared in its modules, impls and traits, in the def map's
/// order; `own` for A's, whose functions get their sites. Each is keyed by its crate's key, its
/// name and which of the crate's bodies of that name it is.
fn collect<A: Allocator>(db: &Db, krate: Crate, own: bool, out: &mut Vec<Body, A>) {
    let key = krate.key(db);
    let start = out.len();
    let other = |def: DefWithBodyId, name: &str| Body {
        def,
        name: name.to_owned(),
        key: BodyKey { krate: key, name: name.to_owned(), nth: 0 },
        own,
        site: None,
    };
    let constant = |it: hir_def::ConstId| {
        let name = ConstSignature::of(db, it).name.as_ref().map_or("_", |it| it.as_str());
        other(it.into(), name)
    };
    for (_, data) in crate_def_map(db, krate).modules(db) {
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(func) => function(db, key, func, own, out),
                ModuleDefId::ConstId(it) => out.push(constant(it)),
                ModuleDefId::StaticId(it) => {
                    out.push(other(it.into(), StaticSignature::of(db, it).name.as_str()))
                }
                ModuleDefId::TraitId(it) => {
                    for &(_, item) in it.trait_items(db).items.iter() {
                        if let AssocItemId::FunctionId(func) = item {
                            function(db, key, func, own, out);
                        }
                    }
                }
                _ => {}
            }
        }
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(func) => function(db, key, func, own, out),
                    AssocItemId::ConstId(it) => out.push(constant(it)),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    }
    // Which of the crate's bodies of its name each is, in the def map's order.
    let mut seen = blaze_hash::HashMap::<String, u32, Global>::new_in(Global);
    for body in &mut out[start..] {
        let nth = seen.entry(body.key.name.clone()).or_insert(0);
        body.key.nth = *nth;
        *nth += 1;
    }
}

/// Adds `func` when it has a body: a trait's function without a default has none. Only an
/// `own` function gets a site.
fn function<A: Allocator>(
    db: &Db,
    krate: CrateKey,
    func: FunctionId,
    own: bool,
    out: &mut Vec<Body, A>,
) {
    let loc = func.lookup(db);
    let source = loc.source_view(db);
    let Some(body) = source.value.body() else { return };
    let signature = FunctionSignature::of(db, func);
    let name = signature.name.as_str().to_owned();
    let curly = body.stmt_list().and_then(|it| it.l_curly_token());
    let site = match (own, source.file_id.file_id(), curly) {
        (true, Some(file), Some(curly)) => Some(Site {
            vfs: file.file_id(db),
            at: curly.text_range().end(),
            is_const: signature.is_const(),
        }),
        _ => None,
    };
    let key = BodyKey { krate, name: name.clone(), nth: 0 };
    out.push(Body { def: func.into(), name, key, own, site });
}
