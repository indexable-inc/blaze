//! Database used for testing `hir_def`: the build's [`Db`], made by `Db::for_tests()`, with the
//! tests' helpers in [`TestDBExt`].

use std::alloc::Global;

use base_db::{
    BuiltCrateData, Committed, Crate, Db, Edit, EditionedFileId, RunOrder, all_crates, relevant_crates,
    set_run_order,
};
use hir_expand::{InFile, files::FilePosition};
use itertools::Itertools;
use span::FileId;
use syntax::view::{self as ast, AstNode};

use crate::{
    Lookup, ModuleDefId, ModuleId,
    expr_store::{Body, scope::ExprScopes},
    nameres::{DefMap, block_def_map, crate_def_map},
    src::HasSource,
};

pub(crate) type TestDB = Db;

pub(crate) trait TestDBExt {
    fn fetch_test_crate(&self) -> Crate;

    fn module_for_file(&self, file_id: FileId) -> ModuleId;

    fn module_at_position(&self, position: FilePosition) -> ModuleId;

    /// The runs of `f`, in the order they started, each `kind(key)`: the query's full name and
    /// its key's `Debug` text, as the run log ([`blaze_memo::testing::RunLog`]) names a row. A
    /// batch's tasks are left to the queries that read their results
    /// ([`RunOrder::LeftToReaders`]), so the order is the one a test expects of the collection
    /// alone.
    fn executed_in_read_order(&self, f: impl FnOnce()) -> Vec<String, Global>;

    /// The runs of `f` as [`TestDBExt::executed_in_read_order`] orders them, each with its
    /// typed key, which [`blaze_memo::testing::Ran::is`] asks.
    fn runs_in_read_order(&self, f: impl FnOnce()) -> Vec<blaze_memo::testing::Ran, Global>;

    /// [`TestDBExt::executed_in_read_order`] of a write and what reads after it: `f` gets the
    /// database to set inputs on, so the runs include what the next snapshot ran for them.
    fn executed_in_write(&mut self, f: impl FnOnce(&mut Db)) -> Vec<String, Global>;

    /// [`TestDBExt::executed_in_write`]'s runs, each with its typed key, which
    /// [`blaze_memo::testing::Ran::is`] asks.
    fn runs_in_write(&mut self, f: impl FnOnce(&mut Db)) -> Vec<blaze_memo::testing::Ran, Global>;

    /// `change` made in one edit ([`Db::edit`]) and committed: what the commit did.
    /// [`Edit::commit`] takes the edit by value, so a setter's `&mut Edit` cannot end a chain.
    fn commit_with(&mut self, change: impl FnOnce(&mut Edit<'_>)) -> Committed;

    /// `file`'s text set to `text` in one edit, committed.
    fn set_file_text(&mut self, file: FileId, text: &str) -> Committed {
        self.commit_with(|edit| _ = edit.set_file_text(file, text))
    }

    /// [`TestDBExt::set_file_text`] of `file`'s file, and `file` again in the new snapshot: an
    /// edit of a file's text drops what is homed with the text, the interned
    /// [`EditionedFileId`] among it, so one held across the commit is dead.
    fn set_text_of(&mut self, file: EditionedFileId, text: &str) -> EditionedFileId;

    /// `krate`'s data set to `data` (each dependency by its key, `BuiltCrateData::keyed`) in
    /// one edit, committed; `krate` again in the new snapshot, as the commit drops its arena.
    fn set_crate_data(&mut self, krate: Crate, data: &BuiltCrateData) -> Crate;
}

/// The runs logged since the last take, as [`TestDBExt::executed_in_read_order`] names them:
/// written inside `attach`, as a run's key is text only when read.
fn named_runs(db: &Db) -> Vec<String, Global> {
    let mut named = Vec::new_in(Global);
    let runs = runs_of(db).take();
    blaze_memo::attach(db, || named.extend(runs.iter().map(|ran| ran.key())));
    named
}

/// `db`'s run log: a test database logs its runs (`Db::for_tests`).
fn runs_of(db: &Db) -> &blaze_memo::testing::RunLog {
    db.runs().expect("a test database logs its runs (`Db::for_tests`)")
}

/// [`blaze_alloc_count::assert_at_most`] of `f` on `db`, a test database, which logs each run:
/// `max` is the budget of `f` itself, and what the log allocated for the runs `f` made is
/// counted around it. A row costs one allocation (its key's `Arc`), and the log's list, taken
/// empty here, its growths: the first row's, and each time the list doubles past 4 rows (a
/// `Vec` of rows this size grows from 0 to 4, then by doubling). A build's database logs
/// nothing; the log is the test's, not the code's.
#[track_caller]
pub(crate) fn assert_at_most_beside_runs<R>(
    db: &Db,
    max: u64,
    what: &str,
    f: impl FnOnce() -> R,
) -> R {
    let log = runs_of(db);
    drop(log.take());
    let (out, used) = blaze_alloc_count::measure(f);
    let rows = log.take().len() as u64;
    let mut grows = 0;
    let mut capacity = 0;
    for row in 1..=rows {
        if row > capacity {
            capacity = (capacity * 2).max(4);
            grows += 1;
        }
    }
    let logged = rows + grows;
    assert!(
        used.allocations <= max + logged,
        "{what}: {} allocations ({} bytes), budget {max} and {logged} of the run log's \
         ({rows} runs)",
        used.allocations,
        used.bytes,
    );
    out
}

impl TestDBExt for Db {
    fn fetch_test_crate(&self) -> Crate {
        let all_crates = all_crates(self);
        all_crates
            .iter()
            .copied()
            .find(|&krate| {
                krate.extra_data(self).display_name.as_ref().map(|it| it.canonical_name().as_str())
                    == Some("ra_test_fixture")
            })
            .unwrap_or(*all_crates.last().unwrap())
    }

    fn module_for_file(&self, file_id: FileId) -> ModuleId {
        for &krate in relevant_crates(self, file_id).iter() {
            let crate_def_map = crate_def_map(self, krate);
            for (local_id, data) in crate_def_map.modules(self) {
                if data.origin.file_id().map(|file_id| file_id.file_id(self)) == Some(file_id) {
                    return local_id;
                }
            }
        }
        panic!("Can't find module for file")
    }

    fn module_at_position(&self, position: FilePosition) -> ModuleId {
        let file_module = self.module_for_file(position.file_id.file_id(self));
        let mut def_map = file_module.def_map(self);
        let module = mod_at_position(self, def_map, position);

        def_map = match block_at_position(self, def_map, position) {
            Some(it) => it,
            None => return module,
        };
        loop {
            let new_map = block_at_position(self, def_map, position);
            match new_map {
                Some(new_block) if !std::ptr::eq(&new_block, &def_map) => {
                    def_map = new_block;
                }
                _ => {
                    // FIXME: handle `mod` inside block expression
                    return def_map.root;
                }
            }
        }
    }

    fn executed_in_read_order(&self, f: impl FnOnce()) -> Vec<String, Global> {
        _ = runs_of(self).take();
        let order = set_run_order(RunOrder::LeftToReaders);
        f();
        set_run_order(order);
        named_runs(self)
    }

    fn runs_in_read_order(&self, f: impl FnOnce()) -> Vec<blaze_memo::testing::Ran, Global> {
        _ = runs_of(self).take();
        let order = set_run_order(RunOrder::LeftToReaders);
        f();
        set_run_order(order);
        runs_of(self).take()
    }

    fn executed_in_write(&mut self, f: impl FnOnce(&mut Db)) -> Vec<String, Global> {
        _ = runs_of(self).take();
        let order = set_run_order(RunOrder::LeftToReaders);
        f(self);
        set_run_order(order);
        named_runs(self)
    }

    fn runs_in_write(&mut self, f: impl FnOnce(&mut Db)) -> Vec<blaze_memo::testing::Ran, Global> {
        _ = runs_of(self).take();
        let order = set_run_order(RunOrder::LeftToReaders);
        f(self);
        set_run_order(order);
        runs_of(self).take()
    }

    fn commit_with(&mut self, change: impl FnOnce(&mut Edit<'_>)) -> Committed {
        let mut edit = self.edit();
        change(&mut edit);
        edit.commit()
    }

    fn set_text_of(&mut self, file: EditionedFileId, text: &str) -> EditionedFileId {
        let span = file.span_file_id(self);
        _ = self.set_file_text(span.file_id(), text);
        EditionedFileId::from_span_file_id(self, span)
    }

    fn set_crate_data(&mut self, krate: Crate, data: &BuiltCrateData) -> Crate {
        let key = krate.key(self);
        let data = data.keyed(self);
        _ = self.commit_with(|edit| _ = edit.set_crate_data(key, data));
        self.krate(key)
    }
}

/// Finds the smallest/innermost module in `def_map` containing `position`.
fn mod_at_position(db: &Db, def_map: &DefMap, position: FilePosition) -> ModuleId {
    let mut size = None;
    let mut res = def_map.root;
    for (module, data) in def_map.modules(db) {
        let src = data.definition_source_view(db);
        // We're not comparing the `base_db::EditionedFileId`, but rather the VFS `FileId`, because
        // `position.file_id` is created before the def map, causing it to have to wrong crate
        // attached often, which means it won't compare equal. This should not be a problem in real
        // r-a session, only in tests, because in real r-a we only guess the crate on syntactic-only
        // (e.g. on-enter) handlers. The rest pick the `EditionedFileId` from the def map.
        let Some(file_id) = src.file_id.file_id() else {
            continue;
        };
        if file_id.file_id(db) != position.file_id.file_id(db) {
            continue;
        }

        let range = src.value.node().text_range();

        if !range.contains(position.offset) {
            continue;
        }

        let new_size = match size {
            None => range.len(),
            Some(size) => {
                if range.len() < size {
                    range.len()
                } else {
                    size
                }
            }
        };

        if size != Some(new_size) {
            size = Some(new_size);
            res = module;
        }
    }

    res
}

fn block_at_position<'db>(
    db: &'db Db,
    def_map: &DefMap,
    position: FilePosition,
) -> Option<&'db DefMap> {
    // Find the smallest (innermost) function in `def_map` containing the cursor.
    let mut size = None;
    let mut fn_def = None;
    for (_, module) in def_map.modules(db) {
        let file_id = module.definition_source_file_id();
        // We're not comparing the `base_db::EditionedFileId`, but rather the VFS `FileId`, because
        // `position.file_id` is created before the def map, causing it to have to wrong crate
        // attached often, which means it won't compare equal. This should not be a problem in real
        // r-a session, only in tests, because in real r-a we only guess the crate on syntactic-only
        // (e.g. on-enter) handlers. The rest pick the `EditionedFileId` from the def map.
        let Some(file_id) = file_id.file_id() else {
            continue;
        };
        if file_id.file_id(db) != position.file_id.file_id(db) {
            continue;
        }
        for decl in module.scope(db).declarations() {
            if let ModuleDefId::FunctionId(it) = decl {
                let range = it.lookup(db).source_view(db).value.syntax().text_range();

                if !range.contains(position.offset) {
                    continue;
                }

                let new_size = match size {
                    None => range.len(),
                    Some(size) => {
                        if range.len() < size {
                            range.len()
                        } else {
                            size
                        }
                    }
                };
                if size != Some(new_size) {
                    size = Some(new_size);
                    fn_def = Some((it, file_id));
                }
            }
        }
    }

    // Find the innermost block expression that has a `DefMap`.
    let (def_with_body, file_id) = fn_def?;
    let def_with_body = def_with_body.into();
    let source_map = Body::source_map(db, def_with_body);
    let scopes = ExprScopes::body_expr_scopes(db, def_with_body);

    let root = file_id.parse(db).root();
    let scope_iter = root
        .token_at_offset(position.offset)
        .map(|token| token.parent_ancestors())
        .kmerge_by(|node1, node2| node1.text_range().len() < node2.text_range().len())
        .filter_map(|node| {
            let block = ast::BlockExpr::cast(node)?;
            let expr = ast::Expr::from(block);
            let expr_id =
                source_map.node_expr_view(InFile::new(file_id.into(), expr))?.as_expr().unwrap();
            let scope = scopes.scope_for(expr_id).unwrap();
            Some(scope)
        });

    for scope in scope_iter {
        let mut containing_blocks =
            scopes.scope_chain(Some(scope)).filter_map(|scope| scopes.block(scope));

        if let Some(block) = containing_blocks.next().map(|block| block_def_map(db, block)) {
            return Some(block);
        }
    }

    None
}
