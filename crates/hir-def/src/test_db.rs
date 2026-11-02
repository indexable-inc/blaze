//! Database used for testing `hir_def`: the build's [`Db`], made by `Db::for_tests()`, with the
//! tests' helpers in [`TestDBExt`].

use base_db::{Crate, Db, RunOrder, all_crates, relevant_crates, set_run_order};
use hir_expand::{InFile, files::FilePosition};
use span::FileId;
use itertools::Itertools;
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

    /// The events of `f`, in the order they happened: a batch's tasks are left to the queries
    /// that read their results ([`RunOrder::LeftToReaders`]), so the order is the one a test
    /// expects of the collection alone.
    fn log_in_read_order(&self, f: impl FnOnce()) -> Vec<blaze_query::db::Event>;

    /// The names of the queries `f` executed, in [`TestDBExt::log_in_read_order`]'s order.
    fn executed_in_read_order(&self, f: impl FnOnce()) -> Vec<String>;

    /// [`TestDBExt::log_in_read_order`] of a write and what reads after it: `f` gets the
    /// database to set inputs on, so the events include what the write-side maintainers ran in
    /// it ([`Db::log_mut`]). Every write ends in them: what a read once ran first, the write
    /// runs now.
    fn log_write_in_read_order(&mut self, f: impl FnOnce(&mut Db)) -> Vec<blaze_query::db::Event>;

    /// The names of the queries `f` executed, in [`TestDBExt::log_write_in_read_order`]'s order.
    fn executed_in_write(&mut self, f: impl FnOnce(&mut Db)) -> Vec<String>;
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

    fn log_in_read_order(&self, f: impl FnOnce()) -> Vec<blaze_query::db::Event> {
        let order = set_run_order(RunOrder::LeftToReaders);
        let events = self.log(f);
        set_run_order(order);
        events
    }

    fn executed_in_read_order(&self, f: impl FnOnce()) -> Vec<String> {
        let order = set_run_order(RunOrder::LeftToReaders);
        let executed = self.log_executed(f);
        set_run_order(order);
        executed
    }

    fn log_write_in_read_order(&mut self, f: impl FnOnce(&mut Db)) -> Vec<blaze_query::db::Event> {
        let order = set_run_order(RunOrder::LeftToReaders);
        let events = self.log_mut(f);
        set_run_order(order);
        events
    }

    fn executed_in_write(&mut self, f: impl FnOnce(&mut Db)) -> Vec<String> {
        let events = self.log_write_in_read_order(f);
        let db: &Db = self;
        events
            .into_iter()
            .filter_map(|event| match event.kind {
                blaze_query::db::EventKind::WillExecute { database_key } => Some(
                    blaze_query::db::Database::storage(db)
                        .ingredient_name(database_key.ingredient_index())
                        .to_owned(),
                ),
                _ => None,
            })
            .collect()
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
    let source_map = &Body::with_source_map(db, def_with_body).1;
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
