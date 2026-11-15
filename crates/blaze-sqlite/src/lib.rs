//! blaze's C SQLite driver (docs/v2/plan/design/sql.md): runs a statement `blaze_sql::check`
//! made on the bundled amalgamation, through `libsqlite3-sys`'s raw FFI and nothing of rusqlite.
//!
//! The driver holds no decision the checker should make, and trusts none it made: [`prepare`]
//! renders the statement for SQLite and compares SQLite's own account of it (placeholders,
//! result columns and their names, one statement and nothing after it) with the checker's
//! description; [`Statement::column`] holds every value SQLite gives to the description's
//! storage and nullability; [`Connection::verify`] holds a database's tables to the schema.
//! Each difference is an [`Error`] naming the statement, table, column or parameter. Nothing is
//! coerced: a placeholder left unbound, a NaN (which SQLite would keep as null), a value of
//! another storage, a step after the statement finished, are each refused.
//!
//! A [`Connection`] is one `sqlite3` handle, opened without SQLite's own mutex: it is neither
//! `Send` nor `Sync`, and every [`Statement`] borrows it, so a statement never outlives the
//! connection it was prepared on and the connection always closes with none left open.
//!
//! Every connection turns on `pragma case_sensitive_like` before anything else runs on it, and
//! reads it back (`'A' like 'a'` is false), or the open fails naming the pragma. SQLite's
//! `like` otherwise ignores ASCII case and Postgres's does not; with it on, a statement of the
//! subset means the same on both engines, and the checker types `like` once for both.
//!
//! The text the driver keeps (a statement's, a name's, a message's) is a [`Text`]: UTF-8 bytes
//! on the global heap, allocated in the one module that names `Global` (`heap`). This crate is
//! a driver at the edge of the program, handed no allocator by its callers; an error is made
//! once and carried out, and a statement's names live as long as the statement.
//!
//! The waits here are SQLite's own file IO and locks, inside its calls (`sqlite3_step` above
//! all); they open no span yet. When `blaze-trace` lands (docs/v2/plan/design/no-tracing.md),
//! each step and prepare opens one named by the statement's text (every-wait-says-what). How a
//! fiber runs a blocking call into C SQLite is bstd's rule for blocking calls, not decided here.
//!
//! [`prepare`]: Connection::prepare

#![feature(allocator_ext)]
#![feature(register_tool)]
#![register_tool(blaze)]

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::ffi::{CStr, c_char, c_int, c_uint};
use core::fmt;
use core::num::TryFromIntError;
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::str::Utf8Error;
use std::alloc::Global;
use std::path::Path;

use blaze_cast::{i128_from_usize, u64_from_usize, usize_from_u32};
use blaze_sql::Checked;
use blaze_sql::ddl;
use blaze_sql::desc::{Backend, Rendered, Returns, Slot, Storage, ValueRef};
use blaze_sql::render;
use blaze_sql::schema::{Schema, Table};
use libsqlite3_sys as ffi;

/// The driver's allocations on the global heap, each made here and nowhere else in the crate:
/// a driver at the edge of the program is handed no allocator, and what it keeps (a statement's
/// text and names, an error) lives until its caller drops it, on no task's lifetime.
mod heap {
    use std::alloc::Global;

    /// An empty list with room for `capacity` items.
    #[expect(
        blaze::hot_alloc,
        reason = "the driver's one list constructor: its callers hand it no allocator, and what it holds lives until the caller drops it"
    )]
    pub(crate) fn list<T>(capacity: usize) -> Vec<T, Global> {
        Vec::with_capacity_in(capacity, Global)
    }

    /// `value` behind a pointer, for an error that holds an error.
    #[expect(
        blaze::hot_alloc,
        reason = "an error that holds two errors is recursive, so it holds them boxed; made once, on a failed `create`"
    )]
    pub(crate) fn boxed<T>(value: T) -> Box<T, Global> {
        Box::new_in(value, Global)
    }
}

/// Owned UTF-8 text: a statement's, a name's or a message's, as the driver keeps it.
///
/// # Invariants
///
/// `Text::bytes` is UTF-8. Upheld by its constructors (`Text::of`, `Text::empty`,
/// `Text::lossy`, `Text::from_utf8`) and by `Text::push_str`, the one way it grows, which
/// appends a whole `str`.
#[derive(PartialEq, Eq)]
pub struct Text {
    bytes: Vec<u8, Global>,
}

impl Text {
    /// A copy of `text`.
    pub fn of(text: &str) -> Text {
        let mut bytes = heap::list(text.len());
        bytes.extend_from_slice(text.as_bytes());
        Text { bytes }
    }

    /// No text.
    pub fn empty() -> Text {
        Text { bytes: heap::list(0) }
    }

    /// `bytes` as text, each run of bytes that is not UTF-8 written as one U+FFFD, as
    /// `String::from_utf8_lossy` writes it.
    pub fn lossy(bytes: &[u8]) -> Text {
        display(&Lossy(bytes))
    }

    /// `bytes` as text, or `bytes` back with why they are not UTF-8.
    fn from_utf8(bytes: Vec<u8, Global>) -> Result<Text, (Vec<u8, Global>, Utf8Error)> {
        match core::str::from_utf8(&bytes) {
            Ok(_) => Ok(Text { bytes }),
            Err(source) => Err((bytes, source)),
        }
    }

    /// Another `Text` of the same text.
    pub fn copy(&self) -> Text {
        Text::of(self.as_str())
    }

    /// Appends `text`.
    pub fn push_str(&mut self, text: &str) {
        self.bytes.extend_from_slice(text.as_bytes());
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: `Text::bytes` is UTF-8 (the `# Invariants` of [`Text`]); upheld by
        // `Text::of`, `Text::empty`, `Text::lossy`, `Text::from_utf8` and `Text::push_str`.
        unsafe { core::str::from_utf8_unchecked(&self.bytes) }
    }
}

impl Deref for Text {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Text {
        Text::of(text)
    }
}

impl PartialEq<str> for Text {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl fmt::Write for Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push_str(text);
        Ok(())
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Bytes shown as text, each run that is not UTF-8 as one U+FFFD.
struct Lossy<'a>(&'a [u8]);

impl fmt::Display for Lossy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                f.write_str(REPLACEMENT)?;
            }
        }
        Ok(())
    }
}

/// What a run of bytes that is not UTF-8 is shown as: Unicode's replacement character, as
/// `String::from_utf8_lossy` shows it.
const REPLACEMENT: &str = "\u{FFFD}";

/// What a parameter or a result column admits: values of `storage`, and null when `nullable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    pub storage: Storage,
    pub nullable: bool,
}

/// A value SQLite gave, by its storage class; the integer is kept to name a boolean out of
/// range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Found {
    Null,
    Integer(i64),
    Real(f64),
    Text { bytes: usize },
    Blob { bytes: usize },
}

/// How what SQLite prepared differs from the checker's description of the statement.
/// Placeholders count from 1, columns from 0.
#[derive(Debug, PartialEq, Eq)]
pub enum PrepareDifference {
    /// The rendered text is not UTF-8.
    NotUtf8 { source: Utf8Error },
    /// The text is longer than SQLite's `int` length.
    TooLong { bytes: usize },
    /// The text held no statement.
    NoStatement,
    /// Text follows the first statement.
    Tail { rest: Text },
    /// SQLite counts `sqlite` placeholders; the rendering binds `slots`.
    ParameterCount { sqlite: c_int, slots: usize },
    /// Placeholder `placeholder` is not written `?placeholder`; `sqlite` is how it is written,
    /// `None` when it is an anonymous `?`.
    PlaceholderName { placeholder: usize, sqlite: Option<Text> },
    /// A slot binds parameter `param`, but the description has `params`.
    SlotParam { placeholder: usize, param: u32, params: usize },
    /// A slot binds a list whole, as an array, which SQLite has no form of.
    ArraySlot { placeholder: usize },
    /// SQLite returns `sqlite` columns; the description says `description`.
    ColumnCount { sqlite: c_int, description: usize },
    /// Result column `index` is named `sqlite` by SQLite and `description` by the checker.
    ColumnName { index: usize, sqlite: Text, description: Text },
}

/// How a database's table differs from the schema's.
#[derive(Debug, PartialEq, Eq)]
pub enum SchemaDifference {
    /// The database has no table of this name.
    Missing,
    /// The database has a table the schema lacks.
    Extra,
    /// The name is a `kind` (a view, a virtual table) in the database, not a table.
    NotATable { kind: Text },
    /// The table is not STRICT, so SQLite would keep a value of any storage in any column.
    NotStrict,
    /// The schema's column is not in the database's table.
    MissingColumn { column: Text },
    /// The database's table has a column after the schema's last.
    ExtraColumn { column: Text },
    /// The column is at `database_at` in the database and `schema_at` in the schema (from 0).
    ColumnAt { column: Text, schema_at: usize, database_at: usize },
    /// The column is generated or hidden in the database.
    Generated { column: Text },
    /// The column is declared as another storage than the schema's.
    Storage(DeclaredType),
    /// The column is nullable in one and `not null` in the other.
    Nullability { column: Text, nullable_in_schema: bool },
    /// The column's place in the primary key (from 1; 0 for none) differs.
    PrimaryKey { column: Text, schema: u32, database: i64 },
}

/// Column `column` is declared `declared` in the database; the schema stores `expected`.
#[derive(Debug, PartialEq, Eq)]
pub struct DeclaredType {
    pub column: Text,
    pub expected: Storage,
    pub declared: Text,
}

/// What SQLite prepared from `statement` differs from the description.
#[derive(Debug)]
pub struct PrepareError {
    pub statement: Text,
    pub difference: PrepareDifference,
}

/// The database's `table` differs from the schema's.
#[derive(Debug)]
pub struct SchemaError {
    pub table: Text,
    pub difference: SchemaDifference,
}

/// Column `column` (at `index`) of a row of `statement` held `got`, which the description does
/// not admit.
#[derive(Debug)]
pub struct ColumnTypeError {
    pub statement: Text,
    pub column: Text,
    pub index: usize,
    pub expected: Expected,
    pub got: Found,
}

/// Why the driver refused or SQLite failed. Each names the statement, table, column or
/// parameter it is about. Placeholders count from 1, columns from 0.
#[derive(Debug)]
pub enum Error {
    /// SQLite answered `call` with result code `code` while running `statement`.
    Sqlite {
        call: &'static str,
        code: c_int,
        code_text: &'static CStr,
        message: Text,
        statement: Text,
    },
    /// The database at `path` did not open.
    Open { path: Text, code: c_int, code_text: &'static CStr, message: Text },
    /// The connection to `path` opened, but `pragma` did not take: `what`.
    Pragma { path: Text, pragma: &'static str, what: Text },
    /// `text`, handed to SQLite as a C string (`what`), holds a NUL byte at `at`.
    Nul { what: &'static str, text: Text, at: usize },
    /// The checker has no SQLite text for the statement.
    Render(Text),
    /// Writing `create table` text for `table` failed.
    Ddl { table: Text, source: fmt::Error },
    /// `create` failed (`create`), and so did rolling its transaction back (`rollback`).
    Rollback { create: Box<Error, Global>, rollback: Box<Error, Global> },
    /// What SQLite prepared differs from the description.
    Prepare(PrepareError),
    /// A table of the database differs from the schema's.
    Schema(SchemaError),
    /// A row held a value the description does not admit.
    ColumnType(ColumnTypeError),
    /// Column `column` (at `index`) of `statement` holds text that is not UTF-8.
    NotUtf8 { statement: Text, column: Text, index: usize, source: Utf8Error },
    /// SQLite gave column `index` of `statement` a storage class (`class`) it does not document.
    StorageClass { statement: Text, index: usize, class: c_int },
    /// A value of storage `got` (`None`: null) was bound to placeholder `placeholder`, which
    /// binds parameter `param` and admits `expected`.
    BindType {
        statement: Text,
        placeholder: usize,
        param: Text,
        expected: Expected,
        got: Option<Storage>,
    },
    /// A NaN was bound to `param`; SQLite would keep it as null.
    BindNan { statement: Text, placeholder: usize, param: Text },
    /// `statement` has no placeholder `placeholder`; it has `count`.
    NoPlaceholder { statement: Text, placeholder: usize, count: usize },
    /// `statement`'s rows have no column `index`; they have `width`.
    NoColumn { statement: Text, index: usize, width: usize },
    /// `statement` was stepped with placeholder `placeholder` (parameter `param`) unbound.
    Unbound { statement: Text, placeholder: usize, param: Text },
    /// `call` is not allowed while `statement` is `state`.
    State { statement: Text, call: &'static str, state: &'static str },
    /// SQLite counted a negative number (`count`) of changed rows.
    Changes { statement: Text, count: i64, source: TryFromIntError },
    /// `what` of `statement`, `value`, is outside the C type SQLite takes it as.
    Range { statement: Text, what: &'static str, value: i128, source: TryFromIntError },
}

/// What one [`Statement::step`] gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A row, read with [`Statement::column`].
    Row,
    /// The statement finished; `changed` is the rows an `insert`, `update` or `delete` changed
    /// (`sqlite3_changes64`), and 0 for a statement that writes nothing.
    Done { changed: u64 },
}

/// One open SQLite database. Not `Send`, not `Sync`: it is opened without SQLite's mutex.
///
/// # Invariants
///
/// `Connection::db` is an open `sqlite3` handle from when the connection is made until its
/// drop, which alone closes it. Upheld by `Connection::open_named`, the one constructor, and
/// `Connection::drop`.
pub struct Connection {
    db: NonNull<ffi::sqlite3>,
}

/// A statement prepared on a [`Connection`], with the checker's description of it.
pub struct Statement<'c> {
    raw: Raw<'c>,
    slots: Box<[Slot], Global>,
    params: Box<[Shape], Global>,
    columns: Box<[Shape], Global>,
    /// `bound[k - 1]`: placeholder `k` has a value since the last reset.
    bound: Vec<bool, Global>,
    state: State,
}

/// A parameter or a result column: its name and what it admits.
struct Shape {
    name: Text,
    expected: Expected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Before the first step: binds are taken.
    Ready,
    /// On a row.
    Row,
    /// Finished; a reset starts it again.
    Done,
    /// A step failed; a reset starts it again.
    Failed,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Ready => "ready (not yet stepped)",
            State::Row => "on a row",
            State::Done => "done (reset it first)",
            State::Failed => "failed (reset it first)",
        }
    }
}

/// A value as SQLite holds it, before it is held to a description.
enum RawValue<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob(&'a [u8]),
}

impl RawValue<'_> {
    fn found(&self) -> Found {
        match self {
            RawValue::Null => Found::Null,
            RawValue::Integer(value) => Found::Integer(*value),
            RawValue::Real(value) => Found::Real(*value),
            RawValue::Text(bytes) => Found::Text { bytes: bytes.len() },
            RawValue::Blob(bytes) => Found::Blob { bytes: bytes.len() },
        }
    }
}

/// One prepared `sqlite3_stmt` and its text; finalized on drop.
///
/// # Invariants
///
/// `Raw::stmt` is a live statement prepared on `Raw::connection`, finalized by the raw's drop
/// and by nothing else. Upheld by `Raw::prepare`, the one constructor, and `Raw::drop`.
struct Raw<'c> {
    stmt: NonNull<ffi::sqlite3_stmt>,
    connection: &'c Connection,
    text: Text,
}

/// A copy of bytes for a C call, with a NUL after them.
///
/// # Invariants
///
/// `CText::bytes` ends in a NUL and holds no other. Upheld by `nul_terminated`, its one
/// constructor.
struct CText {
    bytes: Box<[u8], Global>,
}

impl CText {
    fn as_c_str(&self) -> &CStr {
        // SAFETY: `CText::bytes` ends in its one NUL (the `# Invariants` of [`CText`]); upheld
        // by `nul_terminated`.
        unsafe { CStr::from_bytes_with_nul_unchecked(&self.bytes) }
    }
}

/// The `pragma` that makes `like` compare ASCII case as Postgres's does.
const CASE_SENSITIVE_LIKE: &str = "pragma case_sensitive_like = on";
/// The query that proves [`CASE_SENSITIVE_LIKE`] took: it gives 0 once `like` minds case.
const LIKE_PROBE: &str = "select 'A' like 'a'";

/// The tables, views and virtual tables of every attached schema, by name.
const TABLE_LIST: &str = "select schema, name, type, strict from pragma_table_list order by name";
/// [`TABLE_LIST`]'s column of the schema a table is in.
const LIST_SCHEMA: u32 = 0;
/// [`TABLE_LIST`]'s column of the table's name.
const LIST_NAME: u32 = 1;
/// [`TABLE_LIST`]'s column of what the name is ([`TABLE_KIND`], `view`, `virtual`, ...).
const LIST_KIND: u32 = 2;
/// [`TABLE_LIST`]'s column that is 1 for a STRICT table.
const LIST_STRICT: u32 = 3;

/// A table's columns in order, hidden and generated ones included: the table's name bound to
/// [`XINFO_TABLE`], its schema to [`XINFO_SCHEMA`].
const TABLE_XINFO: &str =
    "select name, type, \"notnull\", pk, hidden from pragma_table_xinfo(?1, ?2)";
/// [`TABLE_XINFO`]'s placeholder of the table's name.
const XINFO_TABLE: c_int = 1;
/// [`TABLE_XINFO`]'s placeholder of the schema the table is in.
const XINFO_SCHEMA: c_int = 2;
/// [`TABLE_XINFO`]'s column of the column's name.
const XINFO_NAME: u32 = 0;
/// [`TABLE_XINFO`]'s column of the column's declared type.
const XINFO_TYPE: u32 = 1;
/// [`TABLE_XINFO`]'s column that is 1 for a `not null` column.
const XINFO_NOT_NULL: u32 = 2;
/// [`TABLE_XINFO`]'s column of the column's place in the primary key, from 1; 0 for none.
const XINFO_KEY: u32 = 3;
/// [`TABLE_XINFO`]'s column that is not 0 for a hidden or generated column.
const XINFO_HIDDEN: u32 = 4;

/// The schema a connection's own tables are in; an attached database is another.
const MAIN_SCHEMA: &str = "main";
/// What [`TABLE_LIST`] calls a table (not a view or a virtual table).
const TABLE_KIND: &str = "table";
/// The prefix of SQLite's own tables (`sqlite_schema`, `sqlite_sequence`), which a schema
/// never holds; SQLite reserves it without regard to case.
const SQLITE_PREFIX: &str = "sqlite_";
/// The other name a STRICT table takes for an integer column, beside [`ddl::SQLITE_INTEGER`].
const SQLITE_INT_ALIAS: &str = "int";
/// How a null is named in a message.
const NULL: &str = "null";
/// Placeholders are numbered in decimal: `?12` is placeholder twelve.
const DECIMAL_RADIX: usize = 10;

/// `SQLITE_UTF8` as the `u8` `sqlite3_bind_text64` takes: the encoding of every text the
/// driver binds, a Rust `str`.
const TEXT_ENCODING: u8 = {
    let [encoding, 0, 0, 0] = ffi::SQLITE_UTF8.to_le_bytes() else {
        panic!("SQLITE_UTF8 does not fit the `u8` sqlite3_bind_text64 takes")
    };
    encoding
};

impl Connection {
    /// A new, empty database in memory.
    pub fn open_memory() -> Result<Connection, Error> {
        Connection::open_named(c":memory:", ":memory:")
    }

    /// The database file at `path`, read-write, created if absent.
    pub fn open(path: &Path) -> Result<Connection, Error> {
        let shown = display(&path.display());
        let name = nul_terminated(path.as_os_str().as_encoded_bytes(), "a database path", &shown)?;
        Connection::open_named(name.as_c_str(), &shown)
    }

    fn open_named(name: &CStr, shown: &str) -> Result<Connection, Error> {
        let mut db: *mut ffi::sqlite3 = ptr::null_mut();
        let flags = ffi::SQLITE_OPEN_READWRITE
            | ffi::SQLITE_OPEN_CREATE
            | ffi::SQLITE_OPEN_NOMUTEX
            | ffi::SQLITE_OPEN_EXRESCODE;
        // SAFETY: `name` is a `CStr`, NUL-terminated (the `# Invariants` of [`CStr`]); upheld by
        // `CStr::from_bytes_with_nul_unchecked` in `CText::as_c_str` and by the `c""` literal.
        // It outlives the call, `db` is a valid place for the handle, and a null VFS name is
        // SQLite's default VFS.
        let code = unsafe { ffi::sqlite3_open_v2(name.as_ptr(), &mut db, flags, ptr::null()) };
        let Some(db) = NonNull::new(db) else {
            return Err(Error::Open {
                path: Text::of(shown),
                code,
                code_text: code_text(code),
                message: Text::of("SQLite could not allocate a handle"),
            });
        };
        // From here the handle closes on drop, also when the open failed: SQLite gives a handle
        // even then, and it must be closed.
        let connection = Connection { db };
        if code != ffi::SQLITE_OK {
            return Err(Error::Open {
                path: Text::of(shown),
                code,
                code_text: code_text(code),
                message: connection.message(),
            });
        }
        connection.case_sensitive_like(shown)?;
        Ok(connection)
    }

    /// Turns on case-sensitive `like`, before anything else runs on the connection, and reads
    /// it back: SQLite ignores a pragma it does not know without an error, so the setting is
    /// proven by `'A' like 'a'` being false, not taken from the pragma's silence.
    fn case_sensitive_like(&self, shown: &str) -> Result<(), Error> {
        let refuse =
            |what: Text| Error::Pragma { path: Text::of(shown), pragma: CASE_SENSITIVE_LIKE, what };
        if let Err(error) = self.execute_script(CASE_SENSITIVE_LIKE) {
            return Err(refuse(display(&error)));
        }
        let mut raw = Raw::prepare(self, Text::of(LIKE_PROBE), 0)?;
        if !raw.step()? {
            return Err(refuse(display(&format_args!("`{LIKE_PROBE}` gave no row"))));
        }
        match raw.integer_at(0)? {
            0 => Ok(()),
            got => Err(refuse(display(&format_args!(
                "`{LIKE_PROBE}` is {got} after it, so `like` still ignores case"
            )))),
        }
    }

    /// Runs `create table` (blaze_sql::ddl, Backend::Sqlite) for every table of `schema`, all
    /// in one transaction: either every table is created or none is.
    pub fn create(&self, schema: &Schema<'_>) -> Result<(), Error> {
        let mut script = Text::empty();
        script.push_str("begin;\n");
        for table in schema.tables {
            ddl::create_table(table, Backend::Sqlite, &mut script)
                .map_err(|source| Error::Ddl { table: Text::of(table.name), source })?;
            script.push_str(";\n");
        }
        script.push_str("commit;");
        let Err(create) = self.execute_script(&script) else {
            return Ok(());
        };
        // SAFETY: `Connection::db` is open (the `# Invariants` of [`Connection`]); upheld by
        // `Connection::open_named`. The call only reads the handle's state.
        let in_transaction = unsafe { ffi::sqlite3_get_autocommit(self.db.as_ptr()) } == 0;
        if in_transaction && let Err(rollback) = self.execute_script("rollback;") {
            return Err(Error::Rollback {
                create: heap::boxed(create),
                rollback: heap::boxed(rollback),
            });
        }
        Err(create)
    }

    /// Runs `sql`, every statement in it, with nothing bound and no rows read. Only
    /// [`Connection::create`], the open and the tests call it: no text decided at run time
    /// reaches it.
    pub(crate) fn execute_script(&self, sql: &str) -> Result<(), Error> {
        let text = nul_terminated(sql.as_bytes(), "a script", sql)?;
        // SAFETY: `Connection::db` is open (the `# Invariants` of [`Connection`]); upheld by
        // `Connection::open_named`. `text` is NUL-terminated and outlives the call; with no
        // callback the callback argument is never read; a null error out-pointer makes SQLite
        // keep the message on the handle, where `failure` reads it.
        let code = unsafe {
            ffi::sqlite3_exec(
                self.db.as_ptr(),
                text.as_c_str().as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if code != ffi::SQLITE_OK {
            return Err(self.failure("sqlite3_exec", code, sql));
        }
        Ok(())
    }

    /// The database's tables equal `schema`: names, column names in order, storage, not null,
    /// primary key, and each table is STRICT. Else an error naming the first difference: the
    /// schema's tables in order, then any table the database has and the schema lacks. Names
    /// match without regard to ASCII case, as SQLite's and the schema's do.
    ///
    /// A Boolean column's `check (.. in (0, 1))` is not read back; [`Statement::column`]
    /// refuses any other integer in it.
    pub fn verify(&self, schema: &Schema<'_>) -> Result<(), Error> {
        let mut tables = heap::list(0);
        self.tables(&mut tables)?;
        for table in schema.tables {
            let refuse =
                |difference| Error::Schema(SchemaError { table: Text::of(table.name), difference });
            let Some(found) =
                tables.iter().find(|found| found.name.eq_ignore_ascii_case(table.name))
            else {
                return Err(refuse(SchemaDifference::Missing));
            };
            if found.kind != TABLE_KIND {
                return Err(refuse(SchemaDifference::NotATable { kind: found.kind.copy() }));
            }
            if !found.strict {
                return Err(refuse(SchemaDifference::NotStrict));
            }
            let mut columns = heap::list(0);
            self.columns(table.name, &mut columns)?;
            compare_columns(table, &columns).map_err(refuse)?;
        }
        for found in &tables {
            if found.kind == TABLE_KIND && schema.table(&found.name).is_none() {
                return Err(Error::Schema(SchemaError {
                    table: found.name.copy(),
                    difference: SchemaDifference::Extra,
                }));
            }
        }
        Ok(())
    }

    /// Pushes onto `into` the tables and views of the `main` schema, by name, without
    /// SQLite's own (`sqlite_..`).
    fn tables(&self, into: &mut Vec<DbTable, Global>) -> Result<(), Error> {
        let mut raw = Raw::prepare(self, Text::of(TABLE_LIST), 0)?;
        while raw.step()? {
            if raw.text_at(LIST_SCHEMA)? != MAIN_SCHEMA {
                continue;
            }
            let name = raw.text_at(LIST_NAME)?;
            if name
                .get(..SQLITE_PREFIX.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SQLITE_PREFIX))
            {
                continue;
            }
            into.push(DbTable {
                name: Text::of(name),
                kind: Text::of(raw.text_at(LIST_KIND)?),
                strict: raw.integer_at(LIST_STRICT)? != 0,
            });
        }
        Ok(())
    }

    /// Pushes onto `into` the columns of `main`'s table `table`, in order, hidden and
    /// generated ones included.
    fn columns(&self, table: &str, into: &mut Vec<DbColumn, Global>) -> Result<(), Error> {
        let mut raw = Raw::prepare(self, Text::of(TABLE_XINFO), 0)?;
        raw.bind(XINFO_TABLE, ValueRef::Text(table))?;
        raw.bind(XINFO_SCHEMA, ValueRef::Text(MAIN_SCHEMA))?;
        while raw.step()? {
            into.push(DbColumn {
                name: Text::of(raw.text_at(XINFO_NAME)?),
                declared: Text::of(raw.text_at(XINFO_TYPE)?),
                not_null: raw.integer_at(XINFO_NOT_NULL)? != 0,
                key: raw.integer_at(XINFO_KEY)?,
                hidden: raw.integer_at(XINFO_HIDDEN)? != 0,
            });
        }
        Ok(())
    }

    /// Renders `checked` (Backend::Sqlite), prepares it, and compares what SQLite says with the
    /// description: placeholders, result columns and their names, and that the text is one
    /// statement with nothing after it.
    pub fn prepare<'c, A: Allocator + Copy>(
        &'c self,
        checked: &Checked<'_, A>,
        list_lens: &[u32],
    ) -> Result<Statement<'c>, Error> {
        let Rendered { text, slots } = render::render(checked, Backend::Sqlite, list_lens, Global)
            .map_err(|error| Error::Render(display(&error)))?;
        let text = Text::from_utf8(text).map_err(|(bytes, source)| {
            Error::Prepare(PrepareError {
                statement: Text::lossy(&bytes),
                difference: PrepareDifference::NotUtf8 { source },
            })
        })?;
        let raw = Raw::prepare(self, text, ffi::SQLITE_PREPARE_PERSISTENT)?;
        let refuse =
            |difference| Error::Prepare(PrepareError { statement: raw.text.copy(), difference });

        let description = checked.description();
        let mut params = heap::list(description.params.len());
        for param in description.params.iter() {
            params.push(Shape {
                name: Text::of(param.name),
                expected: Expected { storage: param.ty.storage, nullable: param.ty.nullable },
            });
        }
        let rows: &[_] = match &description.returns {
            Returns::Rows(rows) => rows,
            Returns::Count => &[],
        };
        let mut columns = heap::list(rows.len());
        for column in rows.iter() {
            columns.push(Shape {
                name: Text::of(column.name),
                expected: Expected { storage: column.ty.storage, nullable: column.ty.nullable },
            });
        }

        let stmt = raw.stmt.as_ptr();
        for (placeholder, slot) in (1usize..).zip(slots.iter()) {
            match *slot {
                Slot::Scalar { param } | Slot::ListItem { param, .. } => {
                    if usize_from_u32(param) >= params.len() {
                        return Err(refuse(PrepareDifference::SlotParam {
                            placeholder,
                            param,
                            params: params.len(),
                        }));
                    }
                }
                Slot::Array { .. } => {
                    return Err(refuse(PrepareDifference::ArraySlot { placeholder }));
                }
            }
        }
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`.
        let sqlite_params = unsafe { ffi::sqlite3_bind_parameter_count(stmt) };
        if i128::from(sqlite_params) != i128_from_usize(slots.len()) {
            return Err(refuse(PrepareDifference::ParameterCount {
                sqlite: sqlite_params,
                slots: slots.len(),
            }));
        }
        for (k, placeholder) in (1..=sqlite_params).zip(1usize..) {
            // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
            // `Raw::prepare`. `k` is within 1..=sqlite3_bind_parameter_count; the name, when
            // there is one, is a NUL-terminated string the statement owns, read here before
            // any other call on it, as [`c_text`] asks.
            let name = unsafe { c_text(ffi::sqlite3_bind_parameter_name(stmt, k)) };
            let name = name.map(CStr::to_bytes);
            if !name.is_some_and(|name| is_placeholder(name, placeholder)) {
                return Err(refuse(PrepareDifference::PlaceholderName {
                    placeholder,
                    sqlite: name.map(Text::lossy),
                }));
            }
        }
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`.
        let sqlite_columns = unsafe { ffi::sqlite3_column_count(stmt) };
        if i128::from(sqlite_columns) != i128_from_usize(columns.len()) {
            return Err(refuse(PrepareDifference::ColumnCount {
                sqlite: sqlite_columns,
                description: columns.len(),
            }));
        }
        for (index, (at, column)) in (0..sqlite_columns).zip(columns.iter().enumerate()) {
            // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
            // `Raw::prepare`. `index` is below sqlite3_column_count; the name is a
            // NUL-terminated string the statement owns, read here before any other call on it,
            // as [`c_text`] asks.
            let name = unsafe { c_text(ffi::sqlite3_column_name(stmt, index)) };
            let Some(name) = name else {
                return Err(self.failure("sqlite3_column_name", ffi::SQLITE_NOMEM, &raw.text));
            };
            if name.to_bytes() != column.name.as_bytes() {
                return Err(refuse(PrepareDifference::ColumnName {
                    index: at,
                    sqlite: Text::lossy(name.to_bytes()),
                    description: column.name.copy(),
                }));
            }
        }

        let mut bound = heap::list(slots.len());
        bound.resize(slots.len(), false);
        Ok(Statement {
            raw,
            slots: slots.into_boxed_slice(),
            params: params.into_boxed_slice(),
            columns: columns.into_boxed_slice(),
            bound,
            state: State::Ready,
        })
    }

    /// SQLite's message for the last failure on this handle.
    fn message(&self) -> Text {
        // SAFETY: `Connection::db` is open (the `# Invariants` of [`Connection`]); upheld by
        // `Connection::open_named`. The message is a NUL-terminated string SQLite owns until
        // the next call on the handle, copied here before any, as [`c_text`] asks.
        let text = unsafe { c_text(ffi::sqlite3_errmsg(self.db.as_ptr())) };
        match text {
            Some(text) => Text::lossy(text.to_bytes()),
            None => Text::of("(SQLite gave no message)"),
        }
    }

    fn failure(&self, call: &'static str, code: c_int, statement: &str) -> Error {
        Error::Sqlite {
            call,
            code,
            code_text: code_text(code),
            message: self.message(),
            statement: Text::of(statement),
        }
    }
}

impl Drop for Connection {
    #[blaze::nopanic]
    fn drop(&mut self) {
        // SAFETY: `Connection::db` is open and closed only here (the `# Invariants` of
        // [`Connection`]); upheld by `Connection::open_named`. Every statement borrows the
        // connection, so none is left unfinalized and `sqlite3_close` does not answer
        // SQLITE_BUSY; there is no caller to hand any other answer to.
        let _ = unsafe { ffi::sqlite3_close(self.db.as_ptr()) };
    }
}

/// A row of `pragma_table_list`.
struct DbTable {
    name: Text,
    kind: Text,
    strict: bool,
}

/// A row of `pragma_table_xinfo`.
struct DbColumn {
    name: Text,
    declared: Text,
    not_null: bool,
    /// The column's place in the primary key, from 1; 0 for none.
    key: i64,
    hidden: bool,
}

fn compare_columns(table: &Table<'_>, found: &[DbColumn]) -> Result<(), SchemaDifference> {
    let mut key_place = 0u32;
    for (at, column) in table.columns.iter().enumerate() {
        let schema_key = if column.primary_key {
            key_place += 1;
            key_place
        } else {
            0
        };
        let named = |found: &DbColumn| found.name.eq_ignore_ascii_case(column.name);
        let Some(here) = found.get(at).filter(|here| named(*here)) else {
            return Err(match found.iter().position(named) {
                Some(database_at) => SchemaDifference::ColumnAt {
                    column: Text::of(column.name),
                    schema_at: at,
                    database_at,
                },
                None => SchemaDifference::MissingColumn { column: Text::of(column.name) },
            });
        };
        if here.hidden {
            return Err(SchemaDifference::Generated { column: Text::of(column.name) });
        }
        if !declares(column.storage, &here.declared) {
            return Err(SchemaDifference::Storage(DeclaredType {
                column: Text::of(column.name),
                expected: column.storage,
                declared: here.declared.copy(),
            }));
        }
        if here.not_null == column.nullable {
            return Err(SchemaDifference::Nullability {
                column: Text::of(column.name),
                nullable_in_schema: column.nullable,
            });
        }
        if here.key != i64::from(schema_key) {
            return Err(SchemaDifference::PrimaryKey {
                column: Text::of(column.name),
                schema: schema_key,
                database: here.key,
            });
        }
    }
    if let Some(extra) = found.get(table.columns.len()) {
        return Err(SchemaDifference::ExtraColumn { column: extra.name.copy() });
    }
    Ok(())
}

/// A STRICT table's declared type stores `storage`: the type `create table` writes for it
/// ([`ddl::type_name`]), and for an integer (and a Boolean, kept as one) also `int`; without
/// regard to ASCII case, as SQLite may answer it in another case than it was written in.
fn declares(storage: Storage, declared: &str) -> bool {
    let name = ddl::type_name(storage, Backend::Sqlite);
    declared.eq_ignore_ascii_case(name)
        || (name == ddl::SQLITE_INTEGER && declared.eq_ignore_ascii_case(SQLITE_INT_ALIAS))
}

impl<'c> Raw<'c> {
    /// Prepares `text`, which must be exactly one statement.
    fn prepare(connection: &'c Connection, text: Text, flags: c_uint) -> Result<Raw<'c>, Error> {
        let Ok(length) = c_int::try_from(text.len()) else {
            let bytes = text.len();
            return Err(Error::Prepare(PrepareError {
                statement: text,
                difference: PrepareDifference::TooLong { bytes },
            }));
        };
        let start = text.as_ptr().cast::<c_char>();
        let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        // SAFETY: `Connection::db` is open (the `# Invariants` of [`Connection`]); upheld by
        // `Connection::open_named`. `start` points at `length` bytes of `text`, which outlives
        // the call (SQLite reads no further than `length`, nor past a NUL); `stmt` and `tail`
        // are valid places for SQLite's answers.
        let code = unsafe {
            ffi::sqlite3_prepare_v3(
                connection.db.as_ptr(),
                start,
                length,
                flags,
                &mut stmt,
                &mut tail,
            )
        };
        if code != ffi::SQLITE_OK {
            // On failure SQLite sets `stmt` to null: nothing to finalize.
            return Err(connection.failure("sqlite3_prepare_v3", code, &text));
        }
        let Some(stmt) = NonNull::new(stmt) else {
            return Err(Error::Prepare(PrepareError {
                statement: text,
                difference: PrepareDifference::NoStatement,
            }));
        };
        // From here the statement is finalized on drop.
        let raw = Raw { stmt, connection, text };
        // `tail` points into `text`, just past the first statement; compared as addresses.
        let used = tail.addr().checked_sub(start.addr());
        let rest = used.and_then(|used| raw.text.get(used..));
        match rest {
            Some("") => Ok(raw),
            Some(rest) => Err(Error::Prepare(PrepareError {
                statement: raw.text.copy(),
                difference: PrepareDifference::Tail { rest: Text::of(rest) },
            })),
            None => Err(Error::Prepare(PrepareError {
                statement: raw.text.copy(),
                difference: PrepareDifference::Tail {
                    rest: Text::of("(SQLite's tail is outside the text)"),
                },
            })),
        }
    }

    /// One step: `true` on a row, `false` when done.
    fn step(&mut self) -> Result<bool, Error> {
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`. It is stepped by one thread: `Connection` is not `Sync`.
        let code = unsafe { ffi::sqlite3_step(self.stmt.as_ptr()) };
        match code {
            ffi::SQLITE_ROW => Ok(true),
            ffi::SQLITE_DONE => Ok(false),
            code => Err(self.connection.failure("sqlite3_step", code, &self.text)),
        }
    }

    /// Binds `value` to placeholder `k`, with no check against a description.
    fn bind(&mut self, k: c_int, value: ValueRef<'_>) -> Result<(), Error> {
        let stmt = self.stmt.as_ptr();
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`. It is not mid-step (the callers bind only before the first step);
        // SQLite answers SQLITE_RANGE for a `k` outside its placeholders. A text's or a blob's
        // pointer is valid for its length for the call, never null (a Rust slice's pointer is
        // not, even when empty, so an empty one binds as empty, not as null), and
        // SQLITE_TRANSIENT makes SQLite copy the bytes before it returns.
        let code = unsafe {
            match value {
                ValueRef::Null => ffi::sqlite3_bind_null(stmt, k),
                ValueRef::Boolean(value) => ffi::sqlite3_bind_int64(stmt, k, i64::from(value)),
                ValueRef::Integer(value) => ffi::sqlite3_bind_int64(stmt, k, value),
                ValueRef::Real(value) => ffi::sqlite3_bind_double(stmt, k, value),
                ValueRef::Text(text) => ffi::sqlite3_bind_text64(
                    stmt,
                    k,
                    text.as_ptr().cast::<c_char>(),
                    u64_from_usize(text.len()),
                    ffi::SQLITE_TRANSIENT(),
                    TEXT_ENCODING,
                ),
                ValueRef::Blob(bytes) => ffi::sqlite3_bind_blob64(
                    stmt,
                    k,
                    bytes.as_ptr().cast(),
                    u64_from_usize(bytes.len()),
                    ffi::SQLITE_TRANSIENT(),
                ),
            }
        };
        if code != ffi::SQLITE_OK {
            return Err(self.connection.failure("sqlite3_bind", code, &self.text));
        }
        Ok(())
    }

    /// Column `index` of the current row as SQLite holds it. The caller is on a row and
    /// `index` is below the statement's column count.
    fn value(&self, index: u32) -> Result<RawValue<'_>, Error> {
        let at = c_int::try_from(index).map_err(|source| Error::Range {
            statement: self.text.copy(),
            what: "a column index",
            value: i128::from(index),
            source,
        })?;
        let stmt = self.stmt.as_ptr();
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`. It is on a row and `at` is below its column count: upheld by
        // `Statement::column`, `Raw::text_at` and `Raw::integer_at`, which read only after a
        // step gave a row, and only columns the statement has.
        let class = unsafe { ffi::sqlite3_column_type(stmt, at) };
        match class {
            ffi::SQLITE_NULL => Ok(RawValue::Null),
            ffi::SQLITE_INTEGER => {
                // SAFETY: `Raw::stmt` is live and on a row (the `# Invariants` of [`Raw`]);
                // upheld above. The value is an integer, read as one.
                let value = unsafe { ffi::sqlite3_column_int64(stmt, at) };
                Ok(RawValue::Integer(value))
            }
            ffi::SQLITE_FLOAT => {
                // SAFETY: `Raw::stmt` is live and on a row (the `# Invariants` of [`Raw`]);
                // upheld above. The value is a real, read as one.
                let value = unsafe { ffi::sqlite3_column_double(stmt, at) };
                Ok(RawValue::Real(value))
            }
            ffi::SQLITE_TEXT => {
                // SAFETY: `Raw::stmt` is live and on a row (the `# Invariants` of [`Raw`]);
                // upheld above. The value is text, read as text, then its length in bytes (in
                // that order, as SQLite documents).
                let (data, length) = unsafe {
                    let data = ffi::sqlite3_column_text(stmt, at);
                    (data.cast::<u8>(), ffi::sqlite3_column_bytes(stmt, at))
                };
                Ok(RawValue::Text(self.bytes(data, length, "sqlite3_column_text")?))
            }
            ffi::SQLITE_BLOB => {
                // SAFETY: `Raw::stmt` is live and on a row (the `# Invariants` of [`Raw`]);
                // upheld above. The value is a blob, read as one, then its length.
                let (data, length) = unsafe {
                    let data = ffi::sqlite3_column_blob(stmt, at);
                    (data.cast::<u8>(), ffi::sqlite3_column_bytes(stmt, at))
                };
                Ok(RawValue::Blob(self.bytes(data, length, "sqlite3_column_blob")?))
            }
            class => Err(Error::StorageClass {
                statement: self.text.copy(),
                index: usize_from_u32(index),
                class,
            }),
        }
    }

    /// The `length` bytes at `data`, which SQLite gave for a column of the current row.
    fn bytes(&self, data: *const u8, length: c_int, call: &'static str) -> Result<&[u8], Error> {
        let Ok(length) = usize::try_from(length) else {
            return Err(self.connection.failure(call, ffi::SQLITE_MISUSE, &self.text));
        };
        if length == 0 {
            // SQLite gives a null pointer for an empty blob.
            return Ok(&[]);
        }
        if data.is_null() {
            // A null pointer for a value that is not empty is SQLite out of memory.
            return Err(self.connection.failure(call, ffi::SQLITE_NOMEM, &self.text));
        }
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`. SQLite keeps these `length` bytes valid until the statement steps,
        // resets or is finalized, or the column is read as another type. Stepping and resetting
        // take `&mut` of the statement and finalizing is its drop, so none happens while the
        // `&self` this slice borrows lives; and a column is only ever read in its own storage
        // class (text as text, a blob as a blob), so it is never converted.
        Ok(unsafe { core::slice::from_raw_parts(data, length) })
    }

    /// Column `index` of the current row, which must be text. For the driver's own queries.
    fn text_at(&self, index: u32) -> Result<&str, Error> {
        match self.value(index)? {
            RawValue::Text(bytes) => core::str::from_utf8(bytes).map_err(|source| Error::NotUtf8 {
                statement: self.text.copy(),
                column: Text::empty(),
                index: usize_from_u32(index),
                source,
            }),
            found @ (RawValue::Null
            | RawValue::Integer(_)
            | RawValue::Real(_)
            | RawValue::Blob(_)) => Err(self.internal_type(index, Storage::Text, found.found())),
        }
    }

    /// Column `index` of the current row, which must be an integer. For the driver's own
    /// queries.
    fn integer_at(&self, index: u32) -> Result<i64, Error> {
        match self.value(index)? {
            RawValue::Integer(value) => Ok(value),
            found
            @ (RawValue::Null | RawValue::Real(_) | RawValue::Text(_) | RawValue::Blob(_)) => {
                Err(self.internal_type(index, Storage::Integer, found.found()))
            }
        }
    }

    fn internal_type(&self, index: u32, storage: Storage, got: Found) -> Error {
        Error::ColumnType(ColumnTypeError {
            statement: self.text.copy(),
            column: Text::empty(),
            index: usize_from_u32(index),
            expected: Expected { storage, nullable: false },
            got,
        })
    }
}

impl Drop for Raw<'_> {
    #[blaze::nopanic]
    fn drop(&mut self) {
        // SAFETY: `Raw::stmt` is live and finalized only here (the `# Invariants` of [`Raw`]);
        // upheld by `Raw::prepare`. Its answer repeats the last step's failure, which that
        // step already returned.
        let _ = unsafe { ffi::sqlite3_finalize(self.stmt.as_ptr()) };
    }
}

impl Statement<'_> {
    /// What binds to each placeholder: `slots()[k - 1]` to placeholder `k`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// Binds placeholder `k` (from 1). Text and blobs are copied by SQLite (SQLITE_TRANSIENT).
    /// The value must be of the parameter's storage, null only where it is nullable, and not a
    /// NaN; only before the first step (after [`Statement::reset`]).
    pub fn bind(&mut self, k: u32, value: ValueRef<'_>) -> Result<(), Error> {
        if self.state != State::Ready {
            return Err(self.state_error(core::any::type_name_of_val(&Self::bind)));
        }
        let placeholder = usize_from_u32(k);
        let no_placeholder = || Error::NoPlaceholder {
            statement: self.raw.text.copy(),
            placeholder,
            count: self.slots.len(),
        };
        let Some(at) = placeholder.checked_sub(1) else {
            return Err(no_placeholder());
        };
        let Some(slot) = self.slots.get(at) else {
            return Err(no_placeholder());
        };
        let Ok(k_c) = c_int::try_from(k) else {
            return Err(no_placeholder());
        };
        let param = match *slot {
            Slot::Scalar { param } | Slot::ListItem { param, .. } => param,
            Slot::Array { .. } => {
                return Err(Error::Prepare(PrepareError {
                    statement: self.raw.text.copy(),
                    difference: PrepareDifference::ArraySlot { placeholder },
                }));
            }
        };
        let Some(shape) = self.params.get(usize_from_u32(param)) else {
            return Err(Error::Prepare(PrepareError {
                statement: self.raw.text.copy(),
                difference: PrepareDifference::SlotParam {
                    placeholder,
                    param,
                    params: self.params.len(),
                },
            }));
        };
        let admitted = value
            .storage()
            .map_or(shape.expected.nullable, |storage| storage == shape.expected.storage);
        if !admitted {
            return Err(Error::BindType {
                statement: self.raw.text.copy(),
                placeholder,
                param: shape.name.copy(),
                expected: shape.expected,
                got: value.storage(),
            });
        }
        if let ValueRef::Real(real) = value
            && real.is_nan()
        {
            return Err(Error::BindNan {
                statement: self.raw.text.copy(),
                placeholder,
                param: shape.name.copy(),
            });
        }
        self.raw.bind(k_c, value)?;
        if let Some(bound) = self.bound.get_mut(at) {
            *bound = true;
        }
        Ok(())
    }

    /// The next row, or the end. The first step needs every placeholder bound; a step after
    /// `Done` or a failure needs a [`Statement::reset`] first (SQLite would otherwise run the
    /// statement again).
    pub fn step(&mut self) -> Result<Step, Error> {
        match self.state {
            State::Ready => {
                if let Some(at) = self.bound.iter().position(|bound| !bound) {
                    let param = match self.slots.get(at) {
                        Some(
                            Slot::Scalar { param }
                            | Slot::ListItem { param, .. }
                            | Slot::Array { param },
                        ) => self.params.get(usize_from_u32(*param)).map(|shape| shape.name.copy()),
                        None => None,
                    };
                    return Err(Error::Unbound {
                        statement: self.raw.text.copy(),
                        placeholder: at + 1,
                        param: param.unwrap_or_else(Text::empty),
                    });
                }
            }
            State::Row => {}
            State::Done | State::Failed => {
                return Err(self.state_error(core::any::type_name_of_val(&Self::step)));
            }
        }
        // Failed until the step answers: a failed step leaves the statement for a reset.
        self.state = State::Failed;
        if self.raw.step()? {
            self.state = State::Row;
            return Ok(Step::Row);
        }
        self.state = State::Done;
        let stmt = self.raw.stmt.as_ptr();
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`.
        if unsafe { ffi::sqlite3_stmt_readonly(stmt) } != 0 {
            return Ok(Step::Done { changed: 0 });
        }
        // SAFETY: `Connection::db` is open (the `# Invariants` of [`Connection`]); upheld by
        // `Connection::open_named`. The count is of the statement that just finished, as
        // nothing else ran on this connection in between (it is not `Sync`, and this is one
        // call).
        let count = unsafe { ffi::sqlite3_changes64(self.raw.connection.db.as_ptr()) };
        let changed = u64::try_from(count).map_err(|source| Error::Changes {
            statement: self.raw.text.copy(),
            count,
            source,
        })?;
        Ok(Step::Done { changed })
    }

    /// Column `index` (from 0) of the current row, checked against the description: null only
    /// where nullable; the storage class the description names (a Boolean is an integer 0 or
    /// 1); text that is UTF-8.
    pub fn column(&self, index: u32) -> Result<ValueRef<'_>, Error> {
        if self.state != State::Row {
            return Err(self.state_error(core::any::type_name_of_val(&Self::column)));
        }
        let Some(shape) = self.columns.get(usize_from_u32(index)) else {
            return Err(Error::NoColumn {
                statement: self.raw.text.copy(),
                index: usize_from_u32(index),
                width: self.columns.len(),
            });
        };
        let value = self.raw.value(index)?;
        let expected = shape.expected;
        let found = value.found();
        let refuse = || {
            Error::ColumnType(ColumnTypeError {
                statement: self.raw.text.copy(),
                column: shape.name.copy(),
                index: usize_from_u32(index),
                expected,
                got: found,
            })
        };
        match value {
            RawValue::Null => {
                if expected.nullable {
                    Ok(ValueRef::Null)
                } else {
                    Err(refuse())
                }
            }
            RawValue::Integer(integer) => match expected.storage {
                Storage::Integer => Ok(ValueRef::Integer(integer)),
                Storage::Boolean => match integer {
                    0 => Ok(ValueRef::Boolean(false)),
                    1 => Ok(ValueRef::Boolean(true)),
                    _ => Err(refuse()),
                },
                Storage::Real | Storage::Text | Storage::Blob => Err(refuse()),
            },
            RawValue::Real(real) => match expected.storage {
                Storage::Real => Ok(ValueRef::Real(real)),
                Storage::Boolean | Storage::Integer | Storage::Text | Storage::Blob => {
                    Err(refuse())
                }
            },
            RawValue::Text(bytes) => match expected.storage {
                Storage::Text => {
                    core::str::from_utf8(bytes).map(ValueRef::Text).map_err(|source| {
                        Error::NotUtf8 {
                            statement: self.raw.text.copy(),
                            column: shape.name.copy(),
                            index: usize_from_u32(index),
                            source,
                        }
                    })
                }
                Storage::Boolean | Storage::Integer | Storage::Real | Storage::Blob => {
                    Err(refuse())
                }
            },
            RawValue::Blob(bytes) => match expected.storage {
                Storage::Blob => Ok(ValueRef::Blob(bytes)),
                Storage::Boolean | Storage::Integer | Storage::Real | Storage::Text => {
                    Err(refuse())
                }
            },
        }
    }

    /// Back to before the first step, binds cleared. After a failed step, SQLite's reset
    /// answers that step's failure again, which the step already returned; it is not returned
    /// twice.
    pub fn reset(&mut self) -> Result<(), Error> {
        let stmt = self.raw.stmt.as_ptr();
        let failed = self.state == State::Failed;
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`. A reset is allowed in any state.
        let code = unsafe { ffi::sqlite3_reset(stmt) };
        // SAFETY: `Raw::stmt` is live (the `# Invariants` of [`Raw`]); upheld by
        // `Raw::prepare`. Clearing the bindings is allowed in any state.
        let cleared = unsafe { ffi::sqlite3_clear_bindings(stmt) };
        self.bound.fill(false);
        self.state = State::Ready;
        if code != ffi::SQLITE_OK && !failed {
            return Err(self.raw.connection.failure("sqlite3_reset", code, &self.raw.text));
        }
        if cleared != ffi::SQLITE_OK {
            return Err(self.raw.connection.failure(
                "sqlite3_clear_bindings",
                cleared,
                &self.raw.text,
            ));
        }
        Ok(())
    }

    fn state_error(&self, call: &'static str) -> Error {
        Error::State { statement: self.raw.text.copy(), call, state: self.state.name() }
    }
}

/// `bytes` and a NUL after them, refusing a NUL among them.
fn nul_terminated(bytes: &[u8], what: &'static str, shown: &str) -> Result<CText, Error> {
    if let Some(at) = bytes.iter().position(|byte| *byte == 0) {
        return Err(Error::Nul { what, text: Text::of(shown), at });
    }
    let mut text = heap::list(bytes.len() + 1);
    text.extend_from_slice(bytes);
    text.push(0);
    Ok(CText { bytes: text.into_boxed_slice() })
}

/// The NUL-terminated string at `text`; `None` for null.
///
/// # Safety
/// `text` is null or points at a NUL-terminated string that stays valid and unchanged for `'a`.
unsafe fn c_text<'a>(text: *const c_char) -> Option<&'a CStr> {
    if text.is_null() {
        return None;
    }
    // SAFETY: `text` is not null (checked above) and points at a NUL-terminated string valid
    // for `'a` (the `# Safety` of [`c_text`]); upheld by the caller.
    Some(unsafe { CStr::from_ptr(text) })
}

/// SQLite's English name for result code `code`.
fn code_text(code: c_int) -> &'static CStr {
    // SAFETY: `ffi::sqlite3_errstr` takes any code and answers a pointer into SQLite's static
    // table of NUL-terminated names, valid for the program's life, as [`c_text`] asks with
    // `'static`; upheld by `ffi::sqlite3_errstr`.
    let text = unsafe { c_text::<'static>(ffi::sqlite3_errstr(code)) };
    text.unwrap_or(c"unknown result code")
}

/// `name` is `?k`: a `?` and `k` in decimal, without leading zeros.
fn is_placeholder(name: &[u8], k: usize) -> bool {
    let Some(digits) = name.strip_prefix(b"?") else {
        return false;
    };
    if digits.first().is_none_or(|first| *first == b'0') {
        return false;
    }
    let mut value: usize = 0;
    for digit in digits {
        if !digit.is_ascii_digit() {
            return false;
        }
        let Some(next) = value
            .checked_mul(DECIMAL_RADIX)
            .and_then(|value| value.checked_add(usize::from(digit - b'0')))
        else {
            return false;
        };
        value = next;
    }
    value == k
}

/// `value` written out as a [`Text`].
fn display(value: &dyn fmt::Display) -> Text {
    let mut text = Text::empty();
    // Writing into a `Text` does not fail.
    let _ = fmt::Write::write_fmt(&mut text, format_args!("{value}"));
    text
}

fn storage_name(storage: Storage) -> &'static str {
    match storage {
        Storage::Boolean => "a boolean (an integer 0 or 1)",
        Storage::Integer => "an integer",
        Storage::Real => "a real",
        Storage::Text => "UTF-8 text",
        Storage::Blob => "a blob",
    }
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(storage_name(self.storage))?;
        if self.nullable {
            f.write_str(" or ")?;
            f.write_str(NULL)?;
        }
        Ok(())
    }
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Found::Null => f.write_str(NULL),
            Found::Integer(value) => write!(f, "the integer {value}"),
            Found::Real(value) => write!(f, "the real {value}"),
            Found::Text { bytes } => write!(f, "text of {bytes} bytes"),
            Found::Blob { bytes } => write!(f, "a blob of {bytes} bytes"),
        }
    }
}

impl fmt::Display for PrepareDifference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrepareDifference::NotUtf8 { source } => {
                write!(f, "the rendered text is not UTF-8: {source}")
            }
            PrepareDifference::TooLong { bytes } => {
                write!(f, "the text is {bytes} bytes, more than SQLite's `int` length holds")
            }
            PrepareDifference::NoStatement => f.write_str("the text holds no statement"),
            PrepareDifference::Tail { rest } => {
                write!(f, "text follows the first statement: `{rest}`; a text is one statement")
            }
            PrepareDifference::ParameterCount { sqlite, slots } => {
                write!(f, "SQLite counts {sqlite} placeholders, but the rendering binds {slots}")
            }
            PrepareDifference::PlaceholderName { placeholder, sqlite: Some(name) } => {
                write!(f, "placeholder {placeholder} is written `{name}`, not `?{placeholder}`")
            }
            PrepareDifference::PlaceholderName { placeholder, sqlite: None } => write!(
                f,
                "placeholder {placeholder} is an anonymous `?` or unused, not `?{placeholder}`"
            ),
            PrepareDifference::SlotParam { placeholder, param, params } => write!(
                f,
                "placeholder {placeholder} binds parameter {param}, but the description has {params}"
            ),
            PrepareDifference::ArraySlot { placeholder } => write!(
                f,
                "placeholder {placeholder} binds a list as an array, which SQLite has no form of"
            ),
            PrepareDifference::ColumnCount { sqlite, description } => {
                write!(f, "SQLite returns {sqlite} columns, but the description has {description}")
            }
            PrepareDifference::ColumnName { index, sqlite, description } => write!(
                f,
                "SQLite names result column {index} `{sqlite}`, but the description names it `{description}`"
            ),
        }
    }
}

impl fmt::Display for SchemaDifference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaDifference::Missing => f.write_str("the database has no such table"),
            SchemaDifference::Extra => f.write_str("the database has this table, the schema does not"),
            SchemaDifference::NotATable { kind } => write!(f, "it is a {kind} in the database, not a table"),
            SchemaDifference::NotStrict => {
                f.write_str("the table is not STRICT, so SQLite would keep a value of any storage in any column")
            }
            SchemaDifference::MissingColumn { column } => {
                write!(f, "column `{column}` is in the schema but not in the database")
            }
            SchemaDifference::ExtraColumn { column } => {
                write!(f, "column `{column}` is in the database but not in the schema")
            }
            SchemaDifference::ColumnAt { column, schema_at, database_at } => write!(
                f,
                "column `{column}` is column {database_at} in the database, column {schema_at} in the schema (from 0)"
            ),
            SchemaDifference::Generated { column } => {
                write!(f, "column `{column}` is generated or hidden in the database")
            }
            SchemaDifference::Storage(declared) => fmt::Display::fmt(declared, f),
            SchemaDifference::Nullability { column, nullable_in_schema: true } => write!(
                f,
                "column `{column}` is nullable in the schema but `not null` in the database"
            ),
            SchemaDifference::Nullability { column, nullable_in_schema: false } => write!(
                f,
                "column `{column}` is `not null` in the schema but nullable in the database"
            ),
            SchemaDifference::PrimaryKey { column, schema, database } => write!(
                f,
                "column `{column}` is at place {database} of the primary key in the database and {schema} in the schema (0: not in it)"
            ),
        }
    }
}

impl fmt::Display for DeclaredType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "column `{}` is declared `{}` in the database, but the schema stores {}",
            self.column,
            self.declared,
            storage_name(self.expected)
        )
    }
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SQLite's account of `{}` differs from the checker's: {}",
            self.statement, self.difference
        )
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "table `{}` differs from the schema: {}", self.table, self.difference)
    }
}

impl fmt::Display for ColumnTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "column `{}` (at {}) of `{}` held {}, but the description admits {}",
            self.column, self.index, self.statement, self.got, self.expected
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite { call, code, code_text, message, statement } => write!(
                f,
                "SQLite answered `{call}` with {code} ({}): {message}; statement: `{statement}`",
                Lossy(code_text.to_bytes())
            ),
            Error::Open { path, code, code_text, message } => write!(
                f,
                "SQLite did not open `{path}`: {code} ({}): {message}",
                Lossy(code_text.to_bytes())
            ),
            Error::Pragma { path, pragma, what } => {
                write!(f, "`{pragma}` did not take on the connection to `{path}`: {what}")
            }
            Error::Nul { what, text, at } => {
                write!(f, "{what} holds a NUL byte at {at}, which a C string cannot: `{text}`")
            }
            Error::Render(message) => write!(f, "no SQLite text for the statement: {message}"),
            Error::Ddl { table, source } => {
                write!(f, "writing `create table` for `{table}` failed: {source}")
            }
            Error::Rollback { create, rollback } => write!(
                f,
                "creating the tables failed ({create}), and rolling back failed too ({rollback})"
            ),
            Error::Prepare(error) => fmt::Display::fmt(error, f),
            Error::Schema(error) => fmt::Display::fmt(error, f),
            Error::ColumnType(error) => fmt::Display::fmt(error, f),
            Error::NotUtf8 { statement, column, index, source } => write!(
                f,
                "column `{column}` (at {index}) of `{statement}` holds text that is not UTF-8: {source}"
            ),
            Error::StorageClass { statement, index, class } => write!(
                f,
                "SQLite gave column {index} of `{statement}` the undocumented storage class {class}"
            ),
            Error::BindType { statement, placeholder, param, expected, got } => {
                let got = got.map_or(NULL, storage_name);
                write!(
                    f,
                    "parameter `${param}` (placeholder {placeholder} of `{statement}`) admits {expected}, not {got}"
                )
            }
            Error::BindNan { statement, placeholder, param } => write!(
                f,
                "parameter `${param}` (placeholder {placeholder} of `{statement}`) was given a NaN, which SQLite would keep as null"
            ),
            Error::NoPlaceholder { statement, placeholder, count } => {
                write!(f, "`{statement}` has no placeholder {placeholder}; it has {count}, from 1")
            }
            Error::NoColumn { statement, index, width } => write!(
                f,
                "the rows of `{statement}` have no column {index}; they have {width}, from 0"
            ),
            Error::Unbound { statement, placeholder, param } => write!(
                f,
                "`{statement}` was stepped with placeholder {placeholder} (parameter `${param}`) unbound; SQLite would read it as null"
            ),
            Error::State { statement, call, state } => {
                write!(f, "`{call}` is not allowed on `{statement}` while it is {state}")
            }
            Error::Changes { statement, count, source } => {
                write!(f, "SQLite counted {count} changed rows for `{statement}`: {source}")
            }
            Error::Range { statement, what, value, source } => write!(
                f,
                "{what} of `{statement}`, {value}, is outside the C type SQLite takes: {source}"
            ),
        }
    }
}

impl std::error::Error for Error {}
