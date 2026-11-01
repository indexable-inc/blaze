//! A body edit touches only its own function (docs/v2/plan/lanes/firewall/notes.md): edits
//! one function body of a crate at a time and checks that the queries executed again are
//! that function's, its file's and the exceptions Rust's semantics need, and that the code
//! compiled again is that function's.
//!
//! [`Harness::new`] lists the crate's bodies, asks for everything a build asks of them (each
//! body's check and code hash, each file module's item checks, and with [`Codegen`] every
//! instance codegen reaches), then asks again in a revision that changed nothing: what
//! executes then executes in every revision, and is reported as [`Report::volatile`]. Each
//! [`Harness::edit`] inserts [`EDIT`] after a body's `{`, asks again with the edited body's
//! requests first, undoes the edit and asks again; each pass's executions are judged
//! ([`Leak`], [`CodeLeak`]).

#![forbid(unsafe_code)]

mod codegen;
mod keys;

use std::fmt;

use base_db::{Crate, Db, FileId};
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use blaze_query::db::{Database as _, DatabaseKeyIndex, Durability, EventKind};
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, Lookup, ModuleDefId, nameres::crate_def_map,
    signatures::FunctionSignature, src::HasSource,
};
use syntax::{TextRange, TextSize};

pub use codegen::Codegen;

use self::keys::{Edited, Interned, Whose};

/// What an edit inserts after a function body's `{`: a statement that changes the body's MIR
/// and code hash and none of the facts other items read of a body (no type an opaque type
/// hides, no binding held across an `.await`, no value of a `const fn`), and that needs no
/// item of any crate.
pub const EDIT: &str = " let _firewall_edit: u32 = 0;";

/// A body of the crate: a function's, a constant's or a static's.
struct Body {
    def: DefWithBodyId,
    name: String,
    /// Where a function's body is written in a file; `None` for a constant, a static and a
    /// function a macro wrote.
    site: Option<Site>,
}

#[derive(Clone, Copy)]
struct Site {
    file: hir_expand::HirFileId,
    vfs: FileId,
    /// The function's syntax, without the edit.
    range: TextRange,
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

/// What one pass of the requests executed and gave.
#[derive(Default)]
struct Pass {
    executed: Vec<(DatabaseKeyIndex, Request)>,
    /// Executed or validated.
    touched: Vec<DatabaseKeyIndex>,
    /// Each body's code hash, in the list's order; `None` where it has none.
    codes: Vec<Option<ContentId>>,
    instances: Option<codegen::Keys>,
}

/// A query of another item executed again by an edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leak {
    /// The edited function.
    pub edited: String,
    /// In the pass after the edit was undone.
    pub undo: bool,
    /// The query, as the query database names it.
    pub query: String,
    pub key: String,
    /// What the pass asked when it executed.
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
    /// Over every pass after an edit or an undo: the keys executed, and executed or validated.
    pub executed: usize,
    pub touched: usize,
    pub leaks: Vec<Leak>,
    pub code: Vec<CodeLeak>,
    /// Queries that executed again in a revision that changed nothing.
    pub volatile: Vec<String>,
    /// Bodies no edit was made in: constants, statics, functions a macro wrote.
    pub unedited: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.leaks.is_empty() && self.code.is_empty() && self.volatile.is_empty()
    }

    /// The queries that leaked, each once, in order.
    pub fn leaked_queries(&self) -> Vec<&str> {
        // alloc: tests only
        let mut queries: Vec<&str> = self.leaks.iter().map(|it| it.query.as_str()).collect();
        queries.sort_unstable();
        queries.dedup();
        queries
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} edits, {} leaks, {} code leaks, {} volatile", self.edits, self.leaks.len(), self.code.len(), self.volatile.len())?;
        for query in &self.volatile {
            writeln!(f, "  volatile: {query}")?;
        }
        for leak in &self.leaks {
            let pass = if leak.undo { "undo of" } else { "edit of" };
            writeln!(f, "  leak: {pass} `{}` executed {} {} (asked by {})", leak.edited, leak.query, leak.key, leak.request)?;
        }
        for leak in &self.code {
            writeln!(f, "  code: {leak:?}")?;
        }
        Ok(())
    }
}

/// The harness over one crate of a database. See the module.
pub struct Harness {
    krate: Crate,
    bodies: Vec<Body>,
    modules: Vec<hir::Module>,
    codegen: Option<Codegen>,
    /// Every key executed or validated by a pass so far: a key executed outside it is work
    /// an edit's new text asked for, not work done again.
    seen: HashSet<DatabaseKeyIndex>,
    volatile: HashSet<DatabaseKeyIndex>,
    interned: Interned,
    /// The cold pass's code hashes and instances: what each undo must give again.
    base: Pass,
}

impl Harness {
    /// Lists `krate`'s bodies, asks for everything once, then once more in a revision that
    /// changed nothing. Keeps every value of `db` from now on
    /// ([`blaze_query::db::Storage::keep_every_value`]).
    pub fn new(db: &mut Db, krate: Crate, codegen: Option<Codegen>) -> Result<Harness, String> {
        db.storage_mut().keep_every_value(true);
        let bodies = bodies(db, krate);
        let modules = hir::check::file_modules(db, hir::Crate::from(krate));
        let mut harness = Harness {
            krate,
            bodies,
            modules,
            codegen,
            seen: HashSet::default(),
            volatile: HashSet::default(),
            interned: Interned::default(),
            base: Pass::default(),
        };
        let cold = harness.pass(db, None)?;
        harness.seen.extend(cold.touched.iter().copied());
        db.storage_mut().synthetic_write(Durability::LOW);
        let warm = harness.pass(db, None)?;
        harness.seen.extend(warm.touched.iter().copied());
        harness.volatile.extend(warm.executed.iter().map(|&(key, _)| key));
        harness.base = cold;
        Ok(harness)
    }

    /// Asks for everything once more, as each pass does, and gives how many keys executed:
    /// what a bench times one pass by.
    pub fn ask_all(&mut self, db: &Db) -> Result<usize, String> {
        let pass = self.pass(db, None)?;
        self.seen.extend(pass.touched.iter().copied());
        Ok(pass.executed.len())
    }

    /// The number of bodies the crate has.
    pub fn body_count(&self) -> usize {
        self.bodies.len()
    }

    /// The functions an edit can be made in, by their place and name.
    pub fn functions(&self) -> impl Iterator<Item = (usize, &str)> {
        self.bodies.iter().enumerate().filter(|(_, it)| it.site.is_some()).map(|(at, it)| (at, it.name.as_str()))
    }

    /// The place of the function named `name` an edit can be made in.
    pub fn function(&self, name: &str) -> Option<usize> {
        self.functions().find(|&(_, it)| it == name).map(|(at, _)| at)
    }

    /// Every `stride`th function edited in turn with [`EDIT`], from the first.
    pub fn run(&mut self, db: &mut Db, stride: usize) -> Result<Report, String> {
        let mut report = Report::default();
        self.report_setup(db, &mut report);
        // alloc: tests only, the functions' places
        let functions: Vec<usize> = self.functions().map(|(at, _)| at).step_by(stride.max(1)).collect();
        for at in functions {
            self.edit(db, at, EDIT, &mut report)?;
        }
        Ok(report)
    }

    /// What the setup found: the volatile queries and the bodies no edit is made in.
    pub fn report_setup(&self, db: &Db, report: &mut Report) {
        let mut volatile: Vec<String> =
            self.volatile.iter().map(|key| db.storage().ingredient_name(key.ingredient_index()).to_owned()).collect(); // alloc: tests only
        volatile.sort_unstable();
        volatile.dedup();
        report.volatile = volatile;
        report.unedited = self.bodies.iter().filter(|it| it.site.is_none()).map(|it| it.name.clone()).collect(); // alloc: clone: tests only
    }

    /// Inserts `text` after the `{` of function `at`'s body, asks again, undoes it and asks
    /// again, and reports what each pass did of other items.
    pub fn edit(&mut self, db: &mut Db, at: usize, text: &str, report: &mut Report) -> Result<(), String> {
        let body = &self.bodies[at];
        let site = body.site.ok_or_else(|| format!("`{}` has no body written in a file", body.name))?; // alloc: an error
        // alloc: tests only, the file's text before and after the edit
        let before = db.file_text(site.vfs).text(db).as_str().to_owned();
        let offset = usize::from(site.at);
        let mut edited_text = String::with_capacity(before.len() + text.len());
        edited_text.push_str(&before[..offset]);
        edited_text.push_str(text);
        edited_text.push_str(&before[offset..]);
        let grown = TextSize::of(text);
        let index = u32::try_from(at).map_err(|_| "more than `u32::MAX` bodies".to_owned())?; // alloc: an error
        let mut edited = Edited {
            def: body.def,
            index,
            file: site.file,
            vfs: site.vfs,
            range: TextRange::new(site.range.start(), site.range.end() + grown),
        };

        db.set_file_text(site.vfs, &edited_text);
        let after_edit = self.pass(db, Some(at))?;
        self.judge(db, &edited, &after_edit, false, report);
        self.judge_code(db, &edited, &self.base, &after_edit, false, site.is_const, report);

        db.set_file_text(site.vfs, &before);
        edited.range = site.range;
        let after_undo = self.pass(db, Some(at))?;
        self.judge(db, &edited, &after_undo, true, report);
        self.judge_code(db, &edited, &after_edit, &after_undo, true, site.is_const, report);
        let name = &self.bodies[at].name;
        for (body, (was, is)) in self.bodies.iter().zip(self.base.codes.iter().zip(&after_undo.codes)) {
            if was != is {
                // clone: tests only, names for the report
                report.code.push(CodeLeak::NotRestored { edited: name.clone(), body: body.name.clone() });
            }
        }

        self.seen.extend(after_edit.touched.iter().chain(&after_undo.touched).copied());
        report.edits += 1;
        report.executed += after_edit.executed.len() + after_undo.executed.len();
        report.touched += after_edit.touched.len() + after_undo.touched.len();
        Ok(())
    }

    /// Everything a build asks of the crate, each request logged: with `first`, that body's
    /// requests first, then the modules' and the other bodies'; without, the modules' first,
    /// so that what a body's request interns is the body's own.
    fn pass(&mut self, db: &Db, first: Option<usize>) -> Result<Pass, String> {
        let mut pass = Pass { codes: vec![None; self.bodies.len()], ..Pass::default() }; // alloc: tests only, a pass's record
        if let Some(first) = first {
            self.ask_body(db, first, &mut pass);
        }
        for (at, module) in self.modules.iter().enumerate() {
            let request = Request::Module(at as u32);
            let events = db.log(|| {
                _ = hir::check::check_module(db, *module, false, &|_: hir::DefWithBody| false);
            });
            record(db, &events, request, &mut pass, None);
        }
        for at in 0..self.bodies.len() {
            if Some(at) != first {
                self.ask_body(db, at, &mut pass);
            }
        }
        if let Some(codegen) = &self.codegen {
            let mut instances = Ok(codegen::Keys::default());
            let events = db.log(|| instances = codegen::instances(db, self.krate, codegen));
            record(db, &events, Request::Codegen, &mut pass, None);
            pass.instances = Some(instances?);
        }
        Ok(pass)
    }

    fn ask_body(&mut self, db: &Db, at: usize, pass: &mut Pass) {
        let def = self.bodies[at].def;
        let index = at as u32;
        let events = db.log(|| {
            _ = hir::check::check_body(db, def, false);
        });
        record(db, &events, Request::Check(index), pass, Some((index, &mut self.interned)));
        let mut code = None;
        let events = db.log(|| code = hir_ty_share::code::of_item(db, def).as_ref().ok().copied());
        record(db, &events, Request::Code(index), pass, Some((index, &mut self.interned)));
        pass.codes[at] = code;
    }

    /// Reports each key `pass` executed again that is neither the edited body's nor its
    /// file's, nor new, nor volatile, nor an exception that applies.
    fn judge(&self, db: &Db, edited: &Edited, pass: &Pass, undo: bool, report: &mut Report) {
        let is_const = self.bodies[edited.index as usize].site.is_some_and(|it| it.is_const);
        for &(key, request) in &pass.executed {
            if !self.seen.contains(&key) || self.volatile.contains(&key) {
                continue;
            }
            match keys::whose(db, &self.interned, edited, key.key_index()) {
                Whose::Own | Whose::File => continue,
                Whose::Other => {}
            }
            let query = db.storage().ingredient_name(key.ingredient_index());
            // E4: a constant's evaluation calls the edited `const fn` and evaluates again; its
            // value is backdated when it is the same.
            if is_const && query.contains("const_eval") {
                continue;
            }
            report.leaks.push(Leak {
                // clone: tests only, names for the report
                edited: self.bodies[edited.index as usize].name.clone(),
                undo,
                query: query.to_owned(),
                key: format!("{key:?}"), // alloc: tests only, a leak's report
                request: self.request_name(request),
            });
        }
    }

    /// Reports each body whose code hash changed from `before` to `after` but the edited one,
    /// the edited one's when it did not change, and each instance whose key changed that is
    /// not the edited body's ([`hir_ty_codegen::v2::HirInstance::of_body`]). A `const fn`'s
    /// callers in constants are E4's, so they are not judged for one.
    #[allow(clippy::too_many_arguments)]
    fn judge_code(
        &self,
        db: &Db,
        edited: &Edited,
        before: &Pass,
        after: &Pass,
        undo: bool,
        is_const: bool,
        report: &mut Report,
    ) {
        let name = &self.bodies[edited.index as usize].name;
        for (at, (was, is)) in before.codes.iter().zip(&after.codes).enumerate() {
            if at == edited.index as usize {
                if was.is_some() && was == is {
                    // clone: tests only, names for the report
                    report.code.push(CodeLeak::Unchanged { edited: name.clone() });
                }
            } else if was != is && !(is_const && !matches!(self.bodies[at].def, DefWithBodyId::FunctionId(_))) {
                // clone: tests only, names for the report
                report.code.push(CodeLeak::Body { edited: name.clone(), undo, body: self.bodies[at].name.clone() });
            }
        }
        let (Some(was), Some(is)) = (&before.instances, &after.instances) else { return };
        let mut push = |instance: &hir_ty_codegen::v2::HirInstance, how| {
            if !instance.of_body(db, edited.def) {
                report.code.push(CodeLeak::Instance {
                    // clone: tests only, names for the report
                    edited: name.clone(),
                    undo,
                    instance: format!("{instance:?}"), // alloc: tests only, a leak's report
                    how,
                });
            }
        };
        for (instance, key) in was {
            match is.get(instance) {
                Some(now) if now == key => {}
                Some(_) => push(instance, "changed"),
                None => push(instance, "went"),
            }
        }
        for instance in is.keys() {
            if !was.contains_key(instance) {
                push(instance, "appeared");
            }
        }
    }

    fn request_name(&self, request: Request) -> String {
        match request {
            Request::Module(at) => format!("the item checks of module {:?}", self.modules[at as usize]), // alloc: tests only
            Request::Check(at) => format!("the check of `{}`", self.bodies[at as usize].name), // alloc: tests only
            Request::Code(at) => format!("the code hash of `{}`", self.bodies[at as usize].name), // alloc: tests only
            Request::Codegen => "codegen's walk".to_owned(), // alloc: tests only
        }
    }
}

/// Adds `events`' executions to `pass` and, for a body's request, the macro calls it interned
/// first to `interned` under the body's place.
fn record(db: &Db, events: &[blaze_query::db::Event], request: Request, pass: &mut Pass, mut body: Option<(u32, &mut Interned)>) {
    for event in events {
        match event.kind {
            EventKind::WillExecute { database_key } => {
                pass.executed.push((database_key, request));
                pass.touched.push(database_key);
            }
            EventKind::DidValidateMemoizedValue { database_key } => pass.touched.push(database_key),
            EventKind::DidInternValue { key, .. } => {
                if let Some((index, interned)) = &mut body
                    && db.storage().ingredient_name(key.ingredient_index()) == "MacroCallId"
                {
                    interned.entry(key.key_index()).or_insert(*index);
                }
            }
            _ => {}
        }
    }
}

/// Every body of `krate` declared in its modules, impls and traits, in the def map's order.
fn bodies(db: &Db, krate: Crate) -> Vec<Body> {
    // alloc: tests only
    let mut out = Vec::new();
    for (_, data) in crate_def_map(db, krate).modules(db) {
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(func) => function(db, func, &mut out),
                ModuleDefId::ConstId(it) => out.push(Body { def: it.into(), name: format!("{it:?}"), site: None }), // alloc: tests only
                ModuleDefId::StaticId(it) => out.push(Body { def: it.into(), name: format!("{it:?}"), site: None }), // alloc: tests only
                ModuleDefId::TraitId(it) => {
                    for &(_, item) in it.trait_items(db).items.iter() {
                        if let AssocItemId::FunctionId(func) = item {
                            function(db, func, &mut out);
                        }
                    }
                }
                _ => {}
            }
        }
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(func) => function(db, func, &mut out),
                    AssocItemId::ConstId(it) => out.push(Body { def: it.into(), name: format!("{it:?}"), site: None }), // alloc: tests only
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    }
    out
}

/// Adds `func` when it has a body: a trait's function without a default has none.
fn function(db: &Db, func: FunctionId, out: &mut Vec<Body>) {
    let loc = func.lookup(db);
    let source = loc.source_view(db);
    let Some(body) = source.value.body() else { return };
    let signature = FunctionSignature::of(db, func);
    let name = signature.name.as_str().to_owned(); // alloc: tests only, a body's name
    let site = match (source.file_id.file_id(), body.stmt_list().and_then(|it| it.l_curly_token())) {
        (Some(file), Some(curly)) => Some(Site {
            file: source.file_id,
            vfs: file.file_id(db),
            range: loc.id.to_range(db),
            at: curly.text_range().end(),
            is_const: signature.is_const(),
        }),
        _ => None,
    };
    out.push(Body { def: func.into(), name, site });
}
