//! The database every expansion test runs on, with a hook that logs which queries ran.

// Each test binary uses part of it.
#![allow(dead_code)]

use std::cell::RefCell;

use blaze_query::db::{Database, Event, EventKind, Id, Storage};

/// A hand-written database: the storage alone.
pub(crate) struct TestDb(Storage);

thread_local! {
    static RUNS: RefCell<Vec<(&'static str, Id)>> = const { RefCell::new(Vec::new()) };
}

impl TestDb {
    pub(crate) fn new() -> TestDb {
        let hook = |storage: &Storage, event: Event| {
            if let EventKind::WillExecute { database_key } = event.kind {
                let name = storage.ingredient_name(database_key.ingredient_index());
                RUNS.with(|runs| runs.borrow_mut().push((name, database_key.key_index())));
            }
        };
        TestDb(Storage::new(Some(Box::new(hook))))
    }
}

impl Database for TestDb {
    fn storage(&self) -> &Storage {
        &self.0
    }

    fn storage_mut(&mut self) -> &mut Storage {
        &mut self.0
    }
}

/// The queries that ran since the last call, by their ingredients' names, in order: queries
/// run on the thread that asks for them, and so does the hook.
pub(crate) fn runs() -> Vec<&'static str> {
    keyed_runs().into_iter().map(|(name, _)| name).collect()
}

/// As [`runs`], with the key each ran for.
pub(crate) fn keyed_runs() -> Vec<(&'static str, Id)> {
    RUNS.with(|runs| std::mem::take(&mut *runs.borrow_mut()))
}
