//! A database for the tests: the build's database, as `Db::for_tests()` makes it. Linking
//! `hir` links hir-ty-mir's MIR table.

use std::{alloc::Global, sync::Arc};

use blaze_memo::testing::Ran;
use hir as _;

pub(crate) type TestDB = base_db::Db;

/// What `work` ran, from the database's run log (a fixture's database logs its runs): the runs
/// before it are taken off first, so none of them is counted as its.
pub(crate) fn runs_of(db: &TestDB, work: impl FnOnce()) -> Vec<Ran, Global> {
    let log = db.runs().expect("a fixture's database logs its runs");
    _ = log.take();
    work();
    log.take()
}

/// Names the files of `db` by `paths` under the rule `key` from the next commit on, as the
/// driver does (`blaze_lower`'s `name_sources`): an input of the session arena, made each time
/// that arena is, under the rule's fact, set in one commit. A rule of the key given before
/// changes nothing. Every id but a file's and a crate key is of the snapshot before; take them
/// again after.
pub(crate) fn name_sources(
    db: &mut TestDB,
    key: &str,
    paths: impl Fn(&base_db::Db, base_db::Crate, base_db::FileId) -> Option<String>
    + Send
    + Sync
    + 'static,
) {
    use hir_ty_mir::mir::location;
    // dyn: the rule, shared by each making of the session arena
    let paths: Arc<dyn Fn(&base_db::Db, base_db::Crate, base_db::FileId) -> Option<String> + Send + Sync> =
        Arc::new(paths);
    let fact = location::source_names_fact(key);
    let key = key.to_owned();
    let mut edit = db.edit();
    edit.set_given(
        None,
        fact,
        Box::new_in(
            move |_: &blaze_memo::Builder, inputs: &blaze_memo::Inputs<'_>| {
                let paths = Arc::clone(&paths);
                location::name_sources(inputs, &key, move |db, krate, file| paths(db, krate, file));
            },
            Global,
        ),
    );
    _ = edit.commit();
}
