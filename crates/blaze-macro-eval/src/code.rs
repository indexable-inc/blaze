//! The code of a proc-macro crate, as what its expansions are kept under ([`RunKey::code`]):
//! everything the macro's MIR, and so what the evaluator runs, is made from that is known
//! before it runs. That is the crate's content ID (its data, environment, cfg, workspace and
//! its dependencies' content IDs, `base_db::Crate::content`), and each file of the source root
//! of every crate it depends on, itself included, by its path and its text's content ID.
//!
//! Files outside those source roots that a crate of the closure reads (`include_str!`,
//! `include!` of `OUT_DIR`) are known only once the queries that read them ran, so they are
//! not in the code. They are reads of each expansion instead ([`included`]), which the store
//! checks as it checks every traced read.
//!
//! [`RunKey::code`]: blaze_usercode::RunKey

use std::path::PathBuf;

use base_db::{Crate, Db, FileText, Nonce, SourceRootId};
use blaze_ids::{ContentId, Hasher};

/// What the code is hashed with: a version of this crate's server and shims, which an
/// expansion depends on as much as on the macro's own code.
const DOMAIN: &str = "blaze v2 macro-eval code 1";

/// The hash of `krate`'s code in `db` as it is now.
pub(crate) fn of(db: &Db, krate: Crate) -> ContentId {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.id(&krate.content(db));
    // alloc: once per proc-macro crate per revision: the closure's source roots and files.
    let mut roots: Vec<SourceRootId> = Vec::new();
    for &dep in krate.transitive_deps(db) {
        let root = db.file_source_root(dep.data(db).root_file_id).source_root_id(db);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots.sort_unstable_by_key(|it| it.0);
    hasher.update(&(roots.len() as u64).to_le_bytes());
    // alloc: as above, a root's files with their paths.
    let mut files: Vec<(String, ContentId)> = Vec::new();
    for root in roots {
        let root = db.source_root(root).source_root_ref(db);
        files.clear();
        files.extend(root.iter().map(|file| {
            // alloc: as above.
            let path = root.path_for_file(&file).expect("a source root lists only files it has paths of").to_string();
            let content = match db.has_file_text(file) {
                true => db.file_text(file).content(db),
                false => FileText::no_text(),
            };
            (path, content)
        }));
        files.sort_unstable();
        hasher.update(&(files.len() as u64).to_le_bytes());
        for (path, content) in &files {
            hasher.part(path.as_bytes()).id(content);
        }
    }
    hasher.finish()
}

/// The files outside the source roots that the crates of `krate`'s closure have read so far:
/// what an expansion is traced to have read besides what its macro read itself.
pub(crate) fn included(db: &Db, krate: Crate, out: &mut Vec<PathBuf>) {
    let closure = krate.transitive_deps(db);
    let files = db.files();
    for &dep in closure {
        for (path, _) in files.included_by(db, dep) {
            out.push(path.into());
        }
    }
    for (by, path) in files.sources_included_by() {
        if closure.contains(&by) {
            out.push(path.into());
        }
    }
    out.sort_unstable();
    out.dedup();
}

/// [`of`], kept for one revision of one database: an expansion asks for it every time, and the
/// closure's files are hashed once a revision.
#[derive(Default)]
pub(crate) struct Memo {
    last: parking_lot::Mutex<Option<((Nonce, blaze_query::db::View), ContentId)>>,
}

impl Memo {
    pub(crate) fn get(&self, db: &Db, krate: Crate) -> ContentId {
        let now = db.nonce_and_revision();
        let last = *self.last.lock();
        if let Some((at, code)) = last
            && at == now
        {
            return code;
        }
        // Hashed with no lock held: the hash asks the database, which may wait on another
        // thread's expansion that waits here.
        let code = of(db, krate);
        *self.last.lock() = Some((now, code));
        code
    }
}
