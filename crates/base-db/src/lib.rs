//! The front end's one database, [`Db`], and the query inputs it holds: files, source
//! roots and the crate graph.

#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod budget_tests;
mod change;
mod content;
pub mod crate_attrs;
mod db;
mod editioned_file_id;
mod extensions;
mod file_set;
mod files;
mod input;
pub mod maintain;
mod named;
mod reparse;
mod settings;
mod source_path;
mod table;
pub mod target;
#[cfg(test)]
mod tests;
pub mod trees;
mod view;

use std::{str::FromStr as _, sync::atomic::AtomicUsize};

pub use crate::{
    change::FileChange,
    content::{CFG_OPTIONS_DOMAIN, InputId, cfg_options_content},
    crate_attrs::{
        CRATE_ATTR_FILE_NAME, CrateAttrError, applied_crate_attr_files, crate_attr_error,
        crate_attr_files, crate_attr_owner,
    },
    db::{Db, Reader, RunOrder, Shared, executions, set_run_order},
    editioned_file_id::EditionedFileId,
    files::{AliasError, ExpansionRead, Files, ReadBy},
    input::{
        BuiltCrateData, BuiltDependency, BuiltFor, Crate, CrateBuilder, CrateBuilderId,
        CrateDataBuilder, CrateDisplayName, CrateGraphBuilder, CrateName, CrateNamed, CrateOrigin,
        CrateType, CrateTypes, CratesIdMap, CratesMap, DependencyBuilder, Env, ExtraCrateData,
        LangCrateOrigin, ProcMacroLoadingError, ProcMacroPaths, ReleaseChannel, SourceRoot,
        SourceRootId, TargetKind, UniqueCrateData, UnnamedCrate,
    },
    maintain::{Implied, Maintained, Maintainer, Reads, Trigger, Written},
    named::NAMED_PATHS_DOMAIN,
    reparse::Reparse,
    settings::{BorrowckMode, FrontSettings},
    view::{ViewChange, ViewError, ViewFile, ViewRoot},
};
pub use crate::{
    file_set::{FileSet, FileSetConfig, FileSetConfigBuilder},
    source_path::{AnchoredPath, AnchoredPathBuf, VfsPath},
};
use blaze_hash::HashSet;
pub use blaze_ids::ContentId;
use blaze_ids::Hasher;
use blaze_query::db::{Durability, Setter};
pub use blaze_query::db::{par_for_each, par_map};
pub use paths::{AbsPath, AbsPathBuf};
pub use semver::{BuildMetadata, Prerelease, Version, VersionReq};
pub use span::FileId;
use std::alloc::Global;
use triomphe::Arc;

pub type FxIndexSet<T> = indexmap::IndexSet<T, blaze_hash::FixedState>;
pub type FxIndexMap<K, V> = indexmap::IndexMap<K, V, blaze_hash::FixedState>;

#[macro_export]
macro_rules! impl_intern_key {
    ($id:ident, $loc:ident) => {
        #[blaze_query::interned(unsafe(no_lifetime))]
        #[derive(PartialOrd, Ord)]
        pub struct $id {
            #[returns(ref)]
            pub loc: $loc,
        }

        // If we derive this the query engine prints the values recursively, and this causes us to blow.
        impl ::std::fmt::Debug for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_tuple(stringify!($id))
                    .field(&format_args!("{:04x}", self.0.as_u32()))
                    .finish()
            }
        }
    };
}

pub const DEFAULT_FILE_TEXT_LRU_CAP: u16 = 16;
pub const DEFAULT_BORROWCK_LRU_CAP: u16 = 2024;

/// The set of roots for crates.io libraries.
/// Files in libraries are assumed to never change.
#[blaze_query::input(singleton, debug)]
pub struct LibraryRoots {
    #[returns(ref)]
    pub roots: HashSet<SourceRootId, Global>,
}

/// The set of "local" (that is, from the current workspace) roots.
/// Files in local roots are assumed to change frequently.
#[blaze_query::input(singleton, debug)]
pub struct LocalRoots {
    #[returns(ref)]
    pub roots: HashSet<SourceRootId, Global>,
}

/// The variables of the process a build is for, which `env!` and `option_env!` read where
/// the crate's own have nothing: those of whoever asked for the build.
/// Its content ID is [`Env::content`] of its variables.
#[blaze_query::input(singleton, debug)]
pub struct ProcessEnv {
    #[returns(ref)]
    #[get(vars_field)]
    #[set(set_vars_field)]
    vars: Env,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl ProcessEnv {
    pub fn vars<'db, Db: ?Sized + blaze_query::db::Database>(self, db: &'db Db) -> &'db Env {
        self.vars_field(db)
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// The variable `key` of the process the build is for.
///
/// Read through a memo a variable ([`process_var`]), never the map itself: a query that
/// reads one variable reads that variable's row, whose value is the same when another
/// variable changes, so the change stops there. Until 2026-11-11 every reader read
/// [`ProcessEnv`]'s one field of all the variables, and a request whose environment differed
/// in any variable, read by nothing, had every reader of any variable (each `env!` and
/// `option_env!` expansion, and all that reads those) verified or run again.
pub fn process_env_var(db: &Db, key: &str) -> Option<String> {
    // clone: the variable's value, as its caller takes it
    process_var(db, intern::Symbol::intern(key), ()).clone()
}

/// The process's variable `name` ([`ProcessEnv`]): the one reader of the map of all of them.
/// Its key covers what it reads (rule keys-cover-every-input): the name is its argument and
/// the map its one tracked read, so it runs again whenever any variable is written and
/// gives the same value, to the same readers, unless `name`'s own changed.
// The `()`: a key of one argument must be an ID of the database, and a name is none; two
// arguments are interned together.
#[blaze_query::tracked(returns(ref))]
fn process_var(db: &Db, name: intern::Symbol, _interned: ()) -> Option<String> {
    ProcessEnv::try_get(db)?.vars(db).get(name.as_str())
}

/// What of the process's variables the database's crates read: each `env!` or `option_env!`
/// the `env!` log has ([`Files::env_read`]) that read the process's variable, by crate and
/// name, sorted. A read by a crate of the standard library, which reads its own variables
/// alone, or of a variable the crate has of its own ([`own_env_var`]), read none of the
/// process's ([`env_var`]). The log is never cleared, so it holds every read any value of the
/// database was made from, and a few more: what a front end kept from one build to the next is
/// keyed by in the process's environment.
pub fn process_env_reads(db: &Db) -> Vec<(Crate, String)> {
    let mut read = db.files().env_read();
    read.retain(|(krate, name)| {
        !krate.data(db).origin.is_lang() && own_env_var(db, *krate, name).is_none()
    });
    read
}

/// `CARGO_PRIMARY_PACKAGE`, which cargo sets to `1` for the crates of the packages a build was
/// asked for (`is_primary_package`). It is of the request, not of the crate: never among a
/// crate's variables ([`Crate::env`]), so no crate's content, identity or image key holds it.
/// A crate sees it through [`own_env_var`], and whatever is keyed by what a crate made covers
/// it through the `env!` log, as the crate's read of it with its value then
/// ([`PrimaryCrates`]).
pub const PRIMARY_PACKAGE: &str = "CARGO_PRIMARY_PACKAGE";

/// The crates of the request's primary packages ([`PRIMARY_PACKAGE`]), sorted. None of the
/// standard library's: it reads its own variables alone, as a std built elsewhere does.
#[blaze_query::input(singleton, debug)]
pub struct PrimaryCrates {
    #[returns(clone)]
    crates: std::sync::Arc<[Crate]>,
}

/// The crates of the primary packages ([`PrimaryCrates`]), sorted.
pub fn primary_crates(db: &Db) -> std::sync::Arc<[Crate]> {
    PrimaryCrates::try_get(db).map_or(std::sync::Arc::default(), |it| it.crates(db))
}

/// Whether `krate` is of a primary package of the build ([`PrimaryCrates`]).
pub fn is_primary(db: &Db, krate: Crate) -> bool {
    !krate.data(db).origin.is_lang()
        && PrimaryCrates::try_get(db).is_some_and(|it| it.crates(db).binary_search(&krate).is_ok())
}

/// Has builds from here on take `crates` as those of the primary packages ([`PrimaryCrates`]).
/// The maintainers run again only when a crate that read [`PRIMARY_PACKAGE`] became primary or
/// stopped being so, as [`set_process_env`] does for the process's variables.
pub fn set_primary_crates(db: &mut Db, mut crates: Vec<Crate>) {
    crates.sort_unstable();
    crates.dedup();
    let crates: std::sync::Arc<[Crate]> = crates.into();
    let read: Vec<Crate> = db
        .files()
        .env_read()
        .into_iter()
        .filter(|(_, name)| name == PRIMARY_PACKAGE)
        .map(|(krate, _)| krate)
        .collect();
    db.write("set_primary_crates", |db| match PrimaryCrates::try_get(&*db) {
        Some(primary) => {
            let was = primary.crates(&*db);
            if was == crates {
                return;
            }
            let moved = read
                .iter()
                .any(|it| was.binary_search(it).is_ok() != crates.binary_search(it).is_ok());
            primary.set_crates(db).to(crates);
            if moved {
                db.note(maintain::Note::Crates);
            }
        }
        None => {
            // MEDIUM: of the request, like the crate graph; set again only when it moves.
            PrimaryCrates::new(db, Durability::MEDIUM, crates);
            db.note(maintain::Note::Crates);
        }
    })
}

/// The variable `name` as `krate` has it of its own: its variables ([`Crate::env`]), then
/// [`PRIMARY_PACKAGE`] when it is primary ([`is_primary`]).
pub fn own_env_var(db: &Db, krate: Crate, name: &str) -> Option<String> {
    krate
        .env(db)
        .get(name)
        .or_else(|| (name == PRIMARY_PACKAGE && is_primary(db, krate)).then(|| "1".to_owned()))
}

/// The variable `name` as `env!` and `option_env!` in `krate` read it, as rustc reads them:
/// what cargo set for the crate ([`own_env_var`]), then the process the build is for. The
/// standard library's crates read their own alone, as a std built elsewhere does: no
/// workspace's environment is in their identity.
pub fn env_var(db: &Db, krate: Crate, name: &str) -> Option<String> {
    let own = own_env_var(db, krate, name);
    match krate.data(db).origin.is_lang() {
        true => own,
        false => own.or_else(|| process_env_var(db, name)),
    }
}

/// The variables a proc macro called from a crate with the variables `env` is given, as rustc
/// gives them: the crate's, and [`PRIMARY_PACKAGE`] when it is `primary`. One rule for the
/// front end ([`macro_env`]) and for whoever works an expansion's reads out again with no
/// database (a unit record's lookup).
pub fn given_macro_env(env: &Env, primary: bool) -> std::borrow::Cow<'_, Env> {
    match primary {
        false => std::borrow::Cow::Borrowed(env),
        true => {
            let mut given = env.clone(); // clone: a primary crate's variables, once per crate
            given.set(PRIMARY_PACKAGE, "1");
            std::borrow::Cow::Owned(given)
        }
    }
}

/// [`given_macro_env`] of `krate` in `db`: what its proc-macro calls are given.
pub fn macro_env(db: &Db, krate: Crate) -> &Env {
    match is_primary(db, krate) {
        false => krate.env(db),
        true => primary_macro_env(db, krate),
    }
}

/// [`macro_env`] of a primary crate, made once per crate.
#[blaze_query::tracked(returns(ref))]
fn primary_macro_env(db: &Db, krate: Crate) -> Env {
    given_macro_env(krate.env(db), true).into_owned()
}

/// Has builds from here on be for a process with the variables `vars`. The maintainers run
/// again only when a variable the crates read ([`process_env_reads`]) has another value: no
/// maintained state was made from any other, and a query that reads one later reads it here.
pub fn set_process_env(db: &mut Db, vars: Env) {
    let read = process_env_reads(db);
    db.write("set_process_env", |db| {
        let content = InputId(vars.content());
        match ProcessEnv::try_get(&*db) {
            Some(env) => {
                if env.content_field(&*db) == content {
                    return;
                }
                let was = env.vars(&*db);
                let moved = read.iter().any(|(_, name)| was.get(name) != vars.get(name));
                env.set_vars_field(db).to(vars);
                env.set_content_field(db).to(content);
                if moved {
                    db.note(maintain::Note::Crates);
                }
            }
            None => {
                // MEDIUM: of the request, like the crate graph; set again only when a
                // variable changes, never by an edit to a source file.
                ProcessEnv::new(db, Durability::MEDIUM, vars, content);
                db.note(maintain::Note::Crates);
            }
        }
    })
}

/// A file's text. Set only through [`Files`] (and the [`Db`] methods that call it),
/// which keeps [`FileText::content`] the content ID of the text and its appended module.
#[blaze_query::input(debug)]
pub struct FileText {
    #[returns(ref)]
    #[get(text_field)]
    #[set(set_text_field)]
    text: syntax::Text,
    pub file_id: FileId,
    /// The module a crate root's text ends with that the compiler wrote, not the file on disk:
    /// blaze's test harness or list of proc macros. rustc injects its own after it applies
    /// the crate's `#![cfg]`, so a root that `#![cfg]` takes away still has it.
    #[default]
    #[returns(copy)]
    #[get(appended_root_module_field)]
    #[set(set_appended_root_module_field)]
    appended_root_module: Option<&'static str>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
    /// What [`EditionedFileId::parse`] makes the text's parse from: set with the text, and not
    /// in the content ID, as the parse is the same whatever it holds.
    #[returns(ref)]
    #[get(reparse_field)]
    #[set(set_reparse_field)]
    reparse: Reparse,
}

impl FileText {
    /// A file text's content ID is of the blaze-vfs ID of its text's bytes
    /// (`ContentId::of(blaze_vfs::FILE_DOMAIN, ..)`), then of its appended root module (0, or 1
    /// and the module), which changes what the crate root is. Building on the vfs ID means a
    /// view's file, whose vfs ID is given and checked, is hashed once. The file's ID and path
    /// are not in it.
    pub const DOMAIN: &'static str = "blaze v2 file text 2";
    /// The content ID a file of a source root has when it has neither text nor bytes (an alias
    /// of a file whose bytes are not UTF-8). A file of the VFS whose bytes are not UTF-8 has
    /// the content ID of its bytes instead ([`Files::file_content`]), which `include_bytes!`
    /// and `include_str!` read.
    pub const NO_TEXT_DOMAIN: &'static str = "blaze v2 file without text 1";

    pub fn content_of(text: &str, appended_root_module: Option<&str>) -> ContentId {
        FileText::content_of_file(
            ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes()),
            appended_root_module,
        )
    }

    /// [`FileText::content_of`] from the blaze-vfs ID of the text's bytes, without reading them.
    pub fn content_of_file(file: ContentId, appended_root_module: Option<&str>) -> ContentId {
        let mut hasher = Hasher::new(FileText::DOMAIN);
        hasher.id(&file).opt_part(appended_root_module);
        hasher.finish()
    }

    /// What [`FileText::content_of_file`] hashes under [`FileText::DOMAIN`], appended to `out`:
    /// for hashing many in one batch.
    pub(crate) fn content_bytes(
        file: &ContentId,
        appended_root_module: Option<&str>,
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(&file.0);
        content::opt_bytes(out, appended_root_module);
    }

    pub fn no_text() -> ContentId {
        files::no_text()
    }

    /// The text, which the file's parse tree shares ([`EditionedFileId::parse`]): the VFS's
    /// bytes as they are, a heap file's allocation or a range of the mapped image.
    pub fn text<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db syntax::Text {
        self.text_field(db)
    }

    /// See the field: the module the compiler appends to a crate root.
    pub fn appended_root_module<Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &Db,
    ) -> Option<&'static str> {
        self.appended_root_module_field(db)
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// Where the lines of a file's text are ([`span::LineIndex`]): made the first time a
/// diagnostic, `line!`/`column!`, a caller location, a proc macro's `Span::line` or debug info
/// asks for a line or a column of the text, once per text, and never on load.
#[blaze_query::tracked(returns(ref))]
pub fn line_index(db: &Db, text: FileText) -> span::LineIndex {
    span::LineIndex::new(text.text(db))
}

/// Why a file `include_bytes!` or `include_str!` names could not be read: the error's kind and
/// the OS's code where it has one, which is what rustc's "couldn't read" message prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadError {
    kind: std::io::ErrorKind,
    os: Option<i32>,
}

impl ReadError {
    pub fn of(error: &std::io::Error) -> ReadError {
        ReadError { kind: error.kind(), os: error.raw_os_error() }
    }
}

impl std::fmt::Display for ReadError {
    /// As `std::io::Error` writes itself: "No such file or directory (os error 2)".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.os {
            Some(code) => std::fmt::Display::fmt(&std::io::Error::from_raw_os_error(code), f),
            None => std::fmt::Display::fmt(&self.kind, f),
        }
    }
}

/// What [`IncludedFile::content_now`] last read of each path, with the generation of the
/// watches outside every view ([`blaze_vfs::outside::generation`]) read before it looked.
static VOUCHED: std::sync::LazyLock<
    blaze_core::lock::Lock<blaze_hash::HashMap<std::path::PathBuf, (u64, ContentId), Global>>,
> = std::sync::LazyLock::new(|| {
    blaze_core::lock::Lock::new("db.included.vouched", blaze_hash::HashMap::new_in(Global))
});

/// The generation of an answer blaze holds itself ([`IncludedFile::hold`]): no watch's.
const HELD: u64 = u64::MAX;

thread_local! {
    /// A planted bug, for a test's control: [`IncludedFile::content_now`] looks at the disk
    /// on every ask, as until 2026-11-12. Never set outside a test.
    #[doc(hidden)]
    pub static LOOKS_EVERY_TIME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The contents of a file named by `include_bytes!` or `include_str!`, or why it can't be read.
#[blaze_query::input(debug)]
pub struct IncludedFile {
    #[returns(ref)]
    #[get(read_field)]
    #[set(set_read_field)]
    read: Result<std::sync::Arc<[u8]>, ReadError>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl IncludedFile {
    /// Of its bytes (1 and the bytes), or of why it could not be read (0 and the error's
    /// text): a message that names the error changes with it.
    pub const DOMAIN: &'static str = "blaze v2 included file 2";

    pub fn content_of(read: Result<&[u8], &ReadError>) -> ContentId {
        let mut hasher = Hasher::new(IncludedFile::DOMAIN);
        match read {
            // alloc: the error's text, on the error path
            Err(error) => hasher.update(&[0]).part(error.to_string().as_bytes()),
            Ok(bytes) => hasher.update(&[1]).part(bytes),
        };
        hasher.finish()
    }

    /// The file at `path` read from disk now, as the front end reads a file outside the VFS
    /// ([`Files::included_file`] reads the path its view has it at).
    pub fn read_now(path: &std::path::Path) -> Result<std::sync::Arc<[u8]>, ReadError> {
        // Read from the disk every time, and counted as a look at it: no watch vouches for
        // the file (`blaze_vfs::outside::looks`).
        blaze_vfs::outside::looked(path);
        std::fs::read(path).map(Into::into).map_err(|it| ReadError::of(&it))
    }

    /// [`IncludedFile::content`] of the file at `path` as it is on disk now
    /// ([`IncludedFile::read_now`]): what a unit record that read it is filed by, and what a
    /// build compares with what it read of the file ([`Files::included_is_read`]).
    ///
    /// The invariant: the disk is looked at only while no watch vouches for the answer kept.
    /// The file's directory is wanted watched ([`blaze_vfs::outside::want`]) the first time it
    /// is asked, and the answer of a look is kept with the generation read before the look;
    /// once a directory at or above the file has been watched without a break since, and
    /// heard nothing, the answer kept is the answer (a file that is not there too: its
    /// directory hears it made). Until 2026-11-12 every ask read the file: each unit of a
    /// build asked of each file its closure includes, and a build that looked at any path no
    /// watch covers is never answered again without a walk (the no-op lane), so reth's build
    /// with nothing changed walked 5,305 units for one path a registry crate's `#[path]`
    /// names that is not there.
    pub fn content_now(path: &std::path::Path) -> ContentId {
        let kept = VOUCHED.lock().get(path).copied();
        if let Some((generation, id)) = kept
            && (generation == HELD || blaze_vfs::outside::unchanged_since(path, generation))
            && !LOOKS_EVERY_TIME.get()
        {
            return id;
        }
        let generation = blaze_vfs::outside::generation();
        // A watch is asked by a path's own bytes and hears nothing of where a link leads:
        // only a file whose name has no link in it (the file itself, when it is there, and
        // its directory) is kept; any other is looked at each time.
        let plain = match std::fs::canonicalize(path) {
            Ok(it) => it == path,
            Err(_) => path.parent().is_some_and(|dir| std::fs::canonicalize(dir).is_ok_and(|it| it == dir)),
        };
        if plain
            && kept.is_none()
            && let Some(dir) = path.parent()
        {
            blaze_vfs::outside::want(dir);
        }
        let id = IncludedFile::content_of(IncludedFile::read_now(path).as_deref());
        if plain {
            // alloc: once per included file, its path as the key of what was read of it
            VOUCHED.lock().insert(path.to_path_buf(), (generation, id));
        }
        id
    }

    /// The file at `path` is one blaze holds the bytes of (a file a build script's run left
    /// in its `OUT_DIR`, which blaze writes there itself from the run's kept result): its
    /// content is `content` until it is held again, and the disk is not looked at for it
    /// ([`IncludedFile::content_now`]). Without this each build looked at every such file a
    /// crate includes (serde's `private.rs`, bindgen's `bindings.rs`: about 25 looks a build
    /// of reth), their directory hearing blaze's own restore of them, so no watch vouched and
    /// the build was never answered again without a walk.
    pub fn hold(path: &std::path::Path, content: ContentId) {
        // alloc: once per held file, its path as the key
        VOUCHED.lock().insert(path.to_path_buf(), (HELD, content));
    }

    pub fn read<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db Result<std::sync::Arc<[u8]>, ReadError> {
        self.read_field(db)
    }

    pub fn bytes<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> Option<&'db std::sync::Arc<[u8]>> {
        self.read_field(db).as_ref().ok()
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

#[blaze_query::input(debug)]
pub struct FileSourceRootInput {
    #[returns(copy)]
    pub source_root_id: SourceRootId,
}

/// A source root. Its content ID is of its files' paths and content IDs and `is_library`
/// ([`SourceRoot::DOMAIN`]), kept by [`Files`] as either changes.
#[blaze_query::input(debug)]
pub struct SourceRootInput {
    #[returns(ref)]
    #[get(source_root_field)]
    #[set(set_source_root_field)]
    source_root: Arc<SourceRoot>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl SourceRootInput {
    pub fn source_root<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> Arc<SourceRoot> {
        // clone: the owned copy this API hands out; a reader that only looks uses `source_root_ref`.
        self.source_root_field(db).clone()
    }

    /// [`SourceRootInput::source_root`] without a clone of the `Arc` (a write to the count
    /// every reader of the root shares).
    pub fn source_root_ref<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db SourceRoot {
        self.source_root_field(db)
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce(usize);

impl Default for Nonce {
    #[inline]
    fn default() -> Self {
        Nonce::new()
    }
}

impl Nonce {
    #[inline]
    pub const fn invalid() -> Nonce {
        Nonce(usize::MAX)
    }

    #[inline]
    pub fn new() -> Nonce {
        Nonce(NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

/// Crate related data shared by the whole workspace.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct CrateWorkspaceData {
    pub target: Result<target::TargetData, target::TargetLoadError>,
    /// Toolchain version used to compile the crate.
    pub toolchain: Option<Version>,
    /// How far the evaluation of one constant of the workspace may go.
    pub const_eval: ConstEvalBudget,
    /// The crates the capability effects (`ambient`, `unsafe`, `global_alloc`; hir-ty-pure's
    /// `effect_attrs`) trust: `trusted` under `[workspace.metadata.blaze]`, sorted by name, each
    /// once. Empty for the toolchain's crates, whose trust is their origin's.
    pub trusted: Box<[TrustedCrate]>,
}

/// A crate a workspace trusts for the capability effects: a `#![blaze::capabilities]` crate may
/// reach its code although that code reaches ambient IO, `unsafe` or the global heap, because a
/// person (`owner`) said why (`reason`).
///
/// ```toml
/// [workspace.metadata.blaze.trusted]
/// bstd = { owner = "runtime", reason = "the capabilities themselves: their syscalls and arenas" }
/// ```
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct TrustedCrate {
    /// The crate's name as code names it (`-` written `_`).
    pub name: Box<str>,
    /// Who answers for it.
    pub owner: Box<str>,
    /// Why it is trusted.
    pub reason: Box<str>,
}

impl TrustedCrate {
    /// The key of the table under `[workspace.metadata.blaze]`.
    pub const KEY: &str = "trusted";
}

/// What one evaluation of a constant, static or other compile-time code may spend before it
/// stops with an error: `const-eval` under `[workspace.metadata.blaze]` in the root manifest.
///
/// ```toml
/// [workspace.metadata.blaze]
/// const-eval = { steps = 4_000_000_000, memory = 2_147_483_648 }
/// ```
///
/// rustc's evaluation has no limit of its own past its `long_running_const_eval` lint (at 2
/// million steps, allowed away by `#[allow]`; a step there is a call or a loop's back edge, not
/// a block), so the default is far beyond what code rustc takes without that `#[allow]` ever
/// spends: it is there to end a loop that never would.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct ConstEvalBudget {
    /// Basic blocks run, every one: a bound on any run, loops or not. rustc's lint counts
    /// fewer, only the blocks that end in a call or close a loop.
    pub steps: u64,
    /// Bytes of the evaluation's stack and heap together.
    pub memory: u64,
}

impl ConstEvalBudget {
    /// The key of the table under `[workspace.metadata.blaze]`.
    pub const KEY: &str = "const-eval";
}

impl Default for ConstEvalBudget {
    fn default() -> Self {
        ConstEvalBudget { steps: 1_000_000_000, memory: 1 << 30 }
    }
}

impl CrateWorkspaceData {
    /// Of the target ([`target::TargetData::content`], or 1 and why it could not be loaded),
    /// the toolchain's version, the const-eval budget, and the trusted crates (how many, then
    /// each one's name, owner and reason).
    pub const DOMAIN: &'static str = "blaze v2 crate workspace data 2";

    pub fn content(&self) -> ContentId {
        let mut hasher = Hasher::new(CrateWorkspaceData::DOMAIN);
        match &self.target {
            Ok(target) => hasher.update(&[0]).id(&target.content()),
            // alloc: the error's text, once per workspace's content ID (setup, an error path).
            Err(error) => hasher.update(&[1]).part(error.to_string().as_bytes()),
        };
        hasher.opt_part(self.toolchain.as_ref().map(Version::to_string));
        hasher
            .update(&self.const_eval.steps.to_le_bytes())
            .update(&self.const_eval.memory.to_le_bytes())
            .update(&(self.trusted.len() as u64).to_le_bytes());
        for trusted in &self.trusted {
            hasher
                .part(trusted.name.as_bytes())
                .part(trusted.owner.as_bytes())
                .part(trusted.reason.as_bytes());
        }
        hasher.finish()
    }

    pub fn is_atleast_187(&self) -> bool {
        const VERSION_187: Version = Version {
            major: 1,
            minor: 87,
            patch: 0,
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
        };
        self.toolchain.as_ref().map_or(false, |v| *v >= VERSION_187)
    }
}

pub fn toolchain_channel(db: &Db, krate: Crate) -> Option<ReleaseChannel> {
    krate.workspace_data(db).toolchain.as_ref().and_then(|v| ReleaseChannel::from_str(&v.pre).ok())
}

/// The kind of an input made from `krate`'s sources (a maintainer's per-crate state), by the
/// crate's origin, which every crate has: `LOW` for a workspace crate (`Local`), `HIGH` for the
/// standard library (`Lang`), rustc's crates and registry, git and vendored dependencies
/// (`Library`). blaze-lower gives a crate the origin that matches its source root (a path
/// package's root is local, every other package's a library's); a registry library's doctest
/// crates are `Library` too (blaze-lower `doctests`). A library crate's input stated `LOW`
/// would have every query over it verify again after each workspace edit.
pub fn crate_durability(db: &Db, krate: Crate) -> Durability {
    match krate.data(db).origin {
        CrateOrigin::Local { .. } => Durability::LOW,
        CrateOrigin::Library { .. } | CrateOrigin::Lang(_) | CrateOrigin::Rustc { .. } => {
            Durability::HIGH
        }
    }
}

#[blaze_query::input(singleton, debug)]
struct AllCrates {
    #[returns(clone)]
    crates: std::sync::Arc<[Crate]>,
    #[returns(copy)]
    content: InputId,
}

/// The crate graph's content ID is of its crates' content IDs, in topological order.
pub const CRATE_GRAPH_DOMAIN: &str = "blaze v2 crate graph 1";

fn crate_graph_content(db: &Db, crates: &[Crate]) -> ContentId {
    let mut hasher = Hasher::new(CRATE_GRAPH_DOMAIN);
    content::len(&mut hasher, crates.len());
    for krate in crates {
        hasher.id(&krate.content(db));
    }
    hasher.finish()
}

/// The content ID of [`all_crates`].
pub fn all_crates_content(db: &Db) -> ContentId {
    match AllCrates::try_get(db) {
        Some(all) => all.content(db).id(),
        None => crate_graph_content(db, &[]),
    }
}

/// Sets the crate graph's content ID from its crates' as they are.
pub(crate) fn refresh_crate_graph_content(db: &mut Db) {
    let Some(all) = AllCrates::try_get(&*db) else { return };
    let content = InputId(crate_graph_content(&*db, &all.crates(&*db)));
    if all.content(&*db) != content {
        all.set_content(db).to(content);
    }
}

pub fn set_all_crates_with_durability(
    db: &mut Db,
    crates: impl IntoIterator<Item = Crate>,
    durability: Durability,
) {
    // alloc: the crate graph's list, once per set (setup).
    let crates: std::sync::Arc<[Crate]> = crates.into_iter().collect();
    db.write_crates("set_all_crates_with_durability", |db| {
        let content = InputId(crate_graph_content(&*db, &crates));
        match AllCrates::try_get(&*db) {
            Some(all) => {
                all.set_crates(db).with_durability(durability).to(crates);
                all.set_content(db).with_durability(durability).to(content);
            }
            None => {
                _ = AllCrates::new(db, durability, crates, content);
            }
        }
    })
}

/// Returns the crates in topological order.
///
/// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
pub fn all_crates(db: &Db) -> std::sync::Arc<[Crate]> {
    AllCrates::try_get(db).map_or(std::sync::Arc::default(), |all_crates| all_crates.crates(db))
}

// FIXME: VFS rewrite should allow us to get rid of this wrapper
#[doc(hidden)]
#[blaze_query::interned]
pub struct InternedSourceRootId {
    #[returns(copy)]
    pub id: SourceRootId,
}

/// Crates whose root file is in `id`.
pub fn source_root_crates(db: &Db, id: SourceRootId) -> &[Crate] {
    #[blaze_query::tracked(returns(deref))]
    pub fn source_root_crates(db: &Db, root: SourceRootInput) -> Box<[Crate]> {
        let crates = AllCrates::get(db).crates(db);
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            let mut found = Vec::new_in(frame);
            found.extend(crates.iter().copied().filter(|&krate| {
                let root_file = krate.data(db).root_file_id;
                db.source_root(db.file_source_root(root_file).source_root_id(db)) == root
            }));
            Box::from(&found[..]) // alloc: the memo's value, made again only when the crate graph changes.
        })
    }
    source_root_crates(db, db.source_root(id))
}

/// The domain of [`crate_root_content`] for a crate whose source root holds another crate's
/// `OUT_DIR`.
pub const OWN_ROOT_DOMAIN: &str = "blaze v2 source root content without other crates' OUT_DIRs 1";

/// The `OUT_DIR`s of the other crates of `krate`'s source root, sorted, each without a
/// trailing separator: the directories of that root whose files `krate` is not made from.
///
/// A build script's run is applied by putting the files it left in `OUT_DIR` into its
/// package's source root (`blaze_lower::Lowered::ran`), where the crates given that `OUT_DIR`
/// (the package's library and binaries) `include!` them. The build script's own crate
/// (`build.rs`) is of the same source root and is given no `OUT_DIR`: those files are what
/// its run makes, never what it is made from. Decided by the crates' env, which the plan
/// sets before any run, so it is the same before a run is applied and after.
#[blaze_query::tracked(returns(deref))]
pub fn other_out_dirs(db: &Db, krate: Crate) -> Box<[String]> {
    let out = |krate: Crate| -> Option<String> {
        let dir = krate.env(db).get("OUT_DIR")?;
        let dir = dir.trim_end_matches(['/', '\\']);
        (!dir.is_empty()).then(|| dir.to_owned()) // alloc: the memo's value, a directory a crate of the root
    };
    let own = out(krate);
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    // alloc: the memo's value, once per crate
    let mut dirs: Vec<String> = source_root_crates(db, root)
        .iter()
        .filter_map(|&other| out(other))
        .filter(|dir| Some(dir) != own.as_ref())
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.into_boxed_slice()
}

/// Whether `path` is a file below one of `dirs` ([`other_out_dirs`]).
pub fn below_any(path: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|dir| {
        path.strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with(['/', '\\']))
    })
}

/// The content of `krate`'s source root as `krate` is made from it: what a key or an identity
/// of the crate holds for "every file of its source root".
///
/// The invariant: it does not change when a build script's run is applied for a crate the
/// run's `OUT_DIR` was not given to.
///
/// The root's own content ID ([`SourceRootInput::content`]) for a crate whose root holds no
/// other crate's `OUT_DIR` (every crate but a build script's, and those of a package whose
/// script is not run for them). For a crate whose root does ([`other_out_dirs`]): each
/// other file's path and content, sorted by path. Until 2026-11-11 every key took the
/// root's content whole, so a library package's build script was keyed by the files its own
/// run left: its unit identity and its image key moved once it had run, and the first build
/// after a cold one compiled each such script again, asked the store for its image and its
/// dependencies', and so read the standard library from images it had resolved a build
/// before (docs/v2/plan/design/first-edit.md).
///
/// Why leaving them out covers every input (rule 21). A crate reads a file of its source
/// root by a path its source or its env gives it. `OUT_DIR` is a directory of the build
/// directory named by a hash of the build's own plan; the crate's env does not hold it
/// (that is what [`other_out_dirs`] selects by) and its source cannot name it before the
/// plan that hashes that source exists.
#[blaze_query::tracked(returns(copy))]
pub fn crate_root_content(db: &Db, krate: Crate) -> ContentId {
    let root = db.source_root(db.file_source_root(krate.data(db).root_file_id).source_root_id(db));
    let others = other_out_dirs(db, krate);
    if others.is_empty() {
        return root.content(db);
    }
    let set = root.source_root_ref(db);
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let mut files = Vec::new_in(frame);
        for file in set.iter() {
            // A file with no path on disk is under no directory.
            let path = set.path_for_file(&file);
            if path.and_then(|it| it.as_path()).is_some_and(|it| below_any(it.as_str(), others)) {
                continue;
            }
            // alloc: a path's text, once per file of a build script's package per write of it
            files.push((path.map(|it| it.to_string()).unwrap_or_default(), db.file_content(file)));
        }
        files.sort_unstable();
        let mut hasher = Hasher::new(OWN_ROOT_DOMAIN);
        hasher.update(&[u8::from(set.is_library)]).update(&(files.len() as u64).to_le_bytes());
        for (path, content) in &files {
            hasher.part(path.as_bytes()).id(content);
        }
        hasher.finish()
    })
}

pub fn relevant_crates(db: &Db, file_id: FileId) -> &[Crate] {
    source_root_crates(db, db.file_source_root(file_id).source_root_id(db))
}
