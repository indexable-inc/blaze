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
//! The error messages are `std::string::String`s on the global heap: this crate is a driver at
//! the edge of the program, not the checker, and an error is made once and carried out.
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
use core::ptr::{self, NonNull};
use std::alloc::Global;
use std::path::Path;

use blaze_sql::Checked;
use blaze_sql::ddl;
use blaze_sql::desc::{Backend, Rendered, Returns, Slot, Storage, ValueRef};
use blaze_sql::render;
use blaze_sql::schema::{Schema, Table};
use libsqlite3_sys as ffi;

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
#[derive(Debug, PartialEq, Eq)]
pub enum PrepareDifference {
    /// The rendered text is not UTF-8.
    NotUtf8,
    /// The text is longer than SQLite's `int` length.
    TooLong { bytes: usize },
    /// The text held no statement.
    NoStatement,
    /// Text follows the first statement.
    Tail { rest: String },
    /// SQLite counts `sqlite` placeholders; the rendering binds `slots`.
    ParameterCount { sqlite: u32, slots: u32 },
    /// Placeholder `placeholder` is not written `?placeholder`; `sqlite` is how it is written,
    /// `None` when it is an anonymous `?`.
    PlaceholderName { placeholder: u32, sqlite: Option<String> },
    /// A slot binds parameter `param`, but the description has `params`.
    SlotParam { placeholder: u32, param: u32, params: u32 },
    /// A slot binds a list whole, as an array, which SQLite has no form of.
    ArraySlot { placeholder: u32 },
    /// SQLite returns `sqlite` columns; the description says `description`.
    ColumnCount { sqlite: u32, description: u32 },
    /// Result column `index` is named `sqlite` by SQLite and `description` by the checker.
    ColumnName { index: u32, sqlite: String, description: String },
}

/// How a database's table differs from the schema's.
#[derive(Debug, PartialEq, Eq)]
pub enum SchemaDifference {
    /// The database has no table of this name.
    Missing,
    /// The database has a table the schema lacks.
    Extra,
    /// The name is a `kind` (a view, a virtual table) in the database, not a table.
    NotATable { kind: String },
    /// The table is not STRICT, so SQLite would keep a value of any storage in any column.
    NotStrict,
    /// The schema's column is not in the database's table.
    MissingColumn { column: String },
    /// The database's table has a column after the schema's last.
    ExtraColumn { column: String },
    /// The column is at `database_at` in the database and `schema_at` in the schema (from 0).
    ColumnAt { column: String, schema_at: usize, database_at: usize },
    /// The column is generated or hidden in the database.
    Generated { column: String },
    /// The column is declared `declared` in the database; the schema stores `expected`.
    Storage { column: String, expected: Storage, declared: String },
    /// The column is nullable in one and `not null` in the other.
    Nullability { column: String, nullable_in_schema: bool },
    /// The column's place in the primary key (from 1; 0 for none) differs.
    PrimaryKey { column: String, schema: u32, database: i64 },
}

/// Why the driver refused or SQLite failed. Each names the statement, table, column or
/// parameter it is about.
#[derive(Debug)]
pub enum Error {
    /// SQLite answered `call` with result code `code` while running `statement`.
    Sqlite {
        call: &'static str,
        code: c_int,
        code_text: &'static str,
        message: String,
        statement: String,
    },
    /// The database at `path` did not open.
    Open { path: String, code: c_int, code_text: &'static str, message: String },
    /// The connection to `path` opened, but `pragma` did not take: `what`.
    Pragma { path: String, pragma: &'static str, what: String },
    /// `text`, handed to SQLite as a C string (`what`), holds a NUL byte at `at`.
    Nul { what: &'static str, text: String, at: usize },
    /// The checker has no SQLite text for the statement.
    Render(String),
    /// Writing `create table` text for `table` failed.
    Ddl { table: String },
    /// `create` failed (`create`), and so did rolling its transaction back (`rollback`).
    Rollback { create: Box<Error>, rollback: Box<Error> },
    /// What SQLite prepared from `statement` differs from the description.
    Prepare { statement: String, difference: PrepareDifference },
    /// The database's `table` differs from the schema's.
    Schema { table: String, difference: SchemaDifference },
    /// Column `column` (at `index`) of a row of `statement` held `got`, which the description
    /// does not admit.
    ColumnType { statement: String, column: String, index: u32, expected: Expected, got: Found },
    /// Column `column` (at `index`) of `statement` holds text that is not UTF-8.
    NotUtf8 { statement: String, column: String, index: u32 },
    /// SQLite gave column `index` of `statement` a storage class (`class`) it does not document.
    StorageClass { statement: String, index: u32, class: c_int },
    /// A value of storage `got` (`None`: null) was bound to placeholder `placeholder`, which
    /// binds parameter `param` and admits `expected`.
    BindType {
        statement: String,
        placeholder: u32,
        param: String,
        expected: Expected,
        got: Option<Storage>,
    },
    /// A NaN was bound to `param`; SQLite would keep it as null.
    BindNan { statement: String, placeholder: u32, param: String },
    /// `statement` has no placeholder `placeholder`; it has `count`, from 1.
    NoPlaceholder { statement: String, placeholder: u32, count: u32 },
    /// `statement`'s rows have no column `index`; they have `width`, from 0.
    NoColumn { statement: String, index: u32, width: u32 },
    /// `statement` was stepped with placeholder `placeholder` (parameter `param`) unbound.
    Unbound { statement: String, placeholder: u32, param: String },
    /// `call` is not allowed while `statement` is `state`.
    State { statement: String, call: &'static str, state: &'static str },
    /// SQLite counted a negative number of changed rows.
    Changes { statement: String, count: i64 },
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
pub struct Connection {
    db: NonNull<ffi::sqlite3>,
}

/// A statement prepared on a [`Connection`], with the checker's description of it.
pub struct Statement<'c> {
    raw: Raw<'c>,
    slots: Vec<Slot, Global>,
    params: Vec<Shape, Global>,
    columns: Vec<Shape, Global>,
    /// `bound[k - 1]`: placeholder `k` has a value since the last reset.
    bound: Vec<bool, Global>,
    state: State,
}

/// A parameter or a result column: its name and what it admits.
struct Shape {
    name: String,
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
struct Raw<'c> {
    stmt: NonNull<ffi::sqlite3_stmt>,
    connection: &'c Connection,
    text: String,
}

impl Connection {
    /// A new, empty database in memory.
    pub fn open_memory() -> Result<Connection, Error> {
        Connection::open_named(c":memory:", ":memory:")
    }

    /// The database file at `path`, read-write, created if absent.
    pub fn open(path: &Path) -> Result<Connection, Error> {
        let shown_text = display(&path.display());
        let name =
            nul_terminated(path.as_os_str().as_encoded_bytes(), "a database path", &shown_text)?;
        // `nul_terminated` ends the bytes with the one NUL it adds and refused any other.
        let name = CStr::from_bytes_with_nul(&name).map_err(|_| Error::Nul {
            what: "a database path",
            text: shown_text.clone(),
            at: 0,
        })?;
        Connection::open_named(name, &shown_text)
    }

    fn open_named(name: &CStr, shown: &str) -> Result<Connection, Error> {
        let mut db: *mut ffi::sqlite3 = ptr::null_mut();
        let flags = ffi::SQLITE_OPEN_READWRITE
            | ffi::SQLITE_OPEN_CREATE
            | ffi::SQLITE_OPEN_NOMUTEX
            | ffi::SQLITE_OPEN_EXRESCODE;
        // SAFETY: `name` is a NUL-terminated string that outlives the call, `db` is a valid
        // place for the handle, and a null VFS name is SQLite's default VFS.
        let code = unsafe { ffi::sqlite3_open_v2(name.as_ptr(), &mut db, flags, ptr::null()) };
        let Some(db) = NonNull::new(db) else {
            return Err(Error::Open {
                path: String::from(shown),
                code,
                code_text: code_text(code),
                message: String::from("SQLite could not allocate a handle"),
            });
        };
        // From here the handle closes on drop, also when the open failed: SQLite gives a handle
        // even then, and it must be closed.
        let connection = Connection { db };
        if code != ffi::SQLITE_OK {
            return Err(Error::Open {
                path: String::from(shown),
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
        const PRAGMA: &str = "pragma case_sensitive_like = on";
        let refuse =
            |what: String| Error::Pragma { path: String::from(shown), pragma: PRAGMA, what };
        if let Err(error) = self.execute_script(PRAGMA) {
            return Err(refuse(display(&error)));
        }
        let mut raw = Raw::prepare(self, String::from("select 'A' like 'a'"), 0)?;
        if !raw.step()? {
            return Err(refuse(String::from("`select 'A' like 'a'` gave no row")));
        }
        match raw.integer_at(0)? {
            0 => Ok(()),
            got => Err(refuse(display(&format_args!(
                "`'A' like 'a'` is {got} after it, so `like` still ignores case"
            )))),
        }
    }

    /// Runs `create table` (blaze_sql::ddl, Backend::Sqlite) for every table of `schema`, all
    /// in one transaction: either every table is created or none is.
    pub fn create(&self, schema: &Schema<'_>) -> Result<(), Error> {
        let mut script = String::new();
        script.push_str("begin;\n");
        for table in schema.tables {
            ddl::create_table(table, Backend::Sqlite, &mut script)
                .map_err(|_| Error::Ddl { table: String::from(table.name) })?;
            script.push_str(";\n");
        }
        script.push_str("commit;");
        let Err(create) = self.execute_script(&script) else {
            return Ok(());
        };
        // SAFETY: `self.db` is open; the call only reads the handle's state.
        let in_transaction = unsafe { ffi::sqlite3_get_autocommit(self.db.as_ptr()) } == 0;
        if in_transaction && let Err(rollback) = self.execute_script("rollback;") {
            return Err(Error::Rollback {
                create: Box::new_in(create, Global),
                rollback: Box::new_in(rollback, Global),
            });
        }
        Err(create)
    }

    /// Runs `sql`, every statement in it, with nothing bound and no rows read. Only
    /// [`Connection::create`] and the tests call it: no text decided at run time reaches it.
    pub(crate) fn execute_script(&self, sql: &str) -> Result<(), Error> {
        let text = nul_terminated(sql.as_bytes(), "a script", sql)?;
        // SAFETY: `self.db` is open; `text` is NUL-terminated and outlives the call; with no
        // callback the callback argument is never read; a null error out-pointer makes SQLite
        // keep the message on the handle, where `failure` reads it.
        let code = unsafe {
            ffi::sqlite3_exec(
                self.db.as_ptr(),
                text.as_ptr().cast::<c_char>(),
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
        let tables = self.tables()?;
        for table in schema.tables {
            let refuse = |difference| Error::Schema { table: String::from(table.name), difference };
            let Some(found) =
                tables.iter().find(|found| found.name.eq_ignore_ascii_case(table.name))
            else {
                return Err(refuse(SchemaDifference::Missing));
            };
            if found.kind != "table" {
                return Err(refuse(SchemaDifference::NotATable { kind: found.kind.clone() }));
            }
            if !found.strict {
                return Err(refuse(SchemaDifference::NotStrict));
            }
            let columns = self.columns(table.name)?;
            compare_columns(table, &columns).map_err(refuse)?;
        }
        for found in &tables {
            if found.kind == "table" && schema.table(&found.name).is_none() {
                return Err(Error::Schema {
                    table: found.name.clone(),
                    difference: SchemaDifference::Extra,
                });
            }
        }
        Ok(())
    }

    /// The tables and views of the `main` schema, by name, without SQLite's own (`sqlite_..`).
    fn tables(&self) -> Result<Vec<DbTable, Global>, Error> {
        let mut raw = Raw::prepare(
            self,
            String::from("select schema, name, type, strict from pragma_table_list order by name"),
            0,
        )?;
        let mut tables = Vec::new_in(Global);
        while raw.step()? {
            if raw.text_at(0)? != "main" {
                continue;
            }
            let name = raw.text_at(1)?;
            if name.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("sqlite_")) {
                continue;
            }
            tables.push(DbTable {
                name: String::from(name),
                kind: String::from(raw.text_at(2)?),
                strict: raw.integer_at(3)? != 0,
            });
        }
        Ok(tables)
    }

    /// The columns of `main`'s table `table`, in order, hidden and generated ones included.
    fn columns(&self, table: &str) -> Result<Vec<DbColumn, Global>, Error> {
        let mut raw = Raw::prepare(
            self,
            String::from(
                "select name, type, \"notnull\", pk, hidden from pragma_table_xinfo(?1, 'main')",
            ),
            0,
        )?;
        raw.bind(1, ValueRef::Text(table))?;
        let mut columns = Vec::new_in(Global);
        while raw.step()? {
            columns.push(DbColumn {
                name: String::from(raw.text_at(0)?),
                declared: String::from(raw.text_at(1)?),
                not_null: raw.integer_at(2)? != 0,
                key: raw.integer_at(3)?,
                hidden: raw.integer_at(4)? != 0,
            });
        }
        Ok(columns)
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
        let text = String::from_utf8(text).map_err(|error| Error::Prepare {
            statement: String::from_utf8_lossy(error.as_bytes()).into_owned(),
            difference: PrepareDifference::NotUtf8,
        })?;
        let raw = Raw::prepare(self, text, ffi::SQLITE_PREPARE_PERSISTENT)?;
        let refuse = |difference| Error::Prepare { statement: raw.text.clone(), difference };

        let description = checked.description();
        let mut params = Vec::with_capacity_in(description.params.len(), Global);
        for param in description.params.iter() {
            params.push(Shape {
                name: String::from(param.name),
                expected: Expected { storage: param.ty.storage, nullable: param.ty.nullable },
            });
        }
        let mut columns = Vec::new_in(Global);
        match &description.returns {
            Returns::Rows(rows) => {
                columns.reserve(rows.len());
                for column in rows.iter() {
                    columns.push(Shape {
                        name: String::from(column.name),
                        expected: Expected {
                            storage: column.ty.storage,
                            nullable: column.ty.nullable,
                        },
                    });
                }
            }
            Returns::Count => {}
        }

        let stmt = raw.stmt.as_ptr();
        for (at, slot) in slots.iter().enumerate() {
            let placeholder = count_u32(at + 1);
            match *slot {
                Slot::Scalar { param } | Slot::ListItem { param, .. } => {
                    if param as usize >= params.len() {
                        return Err(refuse(PrepareDifference::SlotParam {
                            placeholder,
                            param,
                            params: count_u32(params.len()),
                        }));
                    }
                }
                Slot::Array { .. } => {
                    return Err(refuse(PrepareDifference::ArraySlot { placeholder }));
                }
            }
        }
        // SAFETY: `stmt` is a live prepared statement.
        let sqlite_params = unsafe { ffi::sqlite3_bind_parameter_count(stmt) };
        if usize::try_from(sqlite_params).ok() != Some(slots.len()) {
            return Err(refuse(PrepareDifference::ParameterCount {
                sqlite: u32::try_from(sqlite_params).unwrap_or(0),
                slots: count_u32(slots.len()),
            }));
        }
        for at in 0..slots.len() {
            let placeholder = count_u32(at + 1);
            // `at + 1` is at most `sqlite_params`, a `c_int`.
            let k = c_int::try_from(at + 1).unwrap_or(c_int::MAX);
            // SAFETY: `stmt` is live and `k` is within 1..=sqlite3_bind_parameter_count; the
            // name, when there is one, is a NUL-terminated string the statement owns, read here
            // before any other call on it.
            let name = unsafe { c_text(ffi::sqlite3_bind_parameter_name(stmt, k)) };
            if !name.is_some_and(|name| is_placeholder(name, at + 1)) {
                return Err(refuse(PrepareDifference::PlaceholderName {
                    placeholder,
                    sqlite: name.map(|name| String::from_utf8_lossy(name).into_owned()),
                }));
            }
        }
        // SAFETY: `stmt` is a live prepared statement.
        let sqlite_columns = unsafe { ffi::sqlite3_column_count(stmt) };
        if usize::try_from(sqlite_columns).ok() != Some(columns.len()) {
            return Err(refuse(PrepareDifference::ColumnCount {
                sqlite: u32::try_from(sqlite_columns).unwrap_or(0),
                description: count_u32(columns.len()),
            }));
        }
        for (at, column) in columns.iter().enumerate() {
            // `at` is below `sqlite_columns`, a `c_int`.
            let index = c_int::try_from(at).unwrap_or(c_int::MAX);
            // SAFETY: `stmt` is live and `index` is below sqlite3_column_count; the name is a
            // NUL-terminated string the statement owns, read here before any other call on it.
            let name = unsafe { c_text(ffi::sqlite3_column_name(stmt, index)) };
            let Some(name) = name else {
                return Err(self.failure("sqlite3_column_name", ffi::SQLITE_NOMEM, &raw.text));
            };
            if name != column.name.as_bytes() {
                return Err(refuse(PrepareDifference::ColumnName {
                    index: count_u32(at),
                    sqlite: String::from_utf8_lossy(name).into_owned(),
                    description: column.name.clone(),
                }));
            }
        }

        let mut bound = Vec::with_capacity_in(slots.len(), Global);
        bound.resize(slots.len(), false);
        Ok(Statement { raw, slots, params, columns, bound, state: State::Ready })
    }

    /// SQLite's message for the last failure on this handle.
    fn message(&self) -> String {
        // SAFETY: `self.db` is a handle SQLite gave; the message is a NUL-terminated string
        // SQLite owns until the next call on the handle, copied here before any.
        let text = unsafe { c_text(ffi::sqlite3_errmsg(self.db.as_ptr())) };
        match text {
            Some(text) => String::from_utf8_lossy(text).into_owned(),
            None => String::from("(SQLite gave no message)"),
        }
    }

    fn failure(&self, call: &'static str, code: c_int, statement: &str) -> Error {
        Error::Sqlite {
            call,
            code,
            code_text: code_text(code),
            message: self.message(),
            statement: String::from(statement),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: `self.db` is open and closed only here. Every statement borrows the
        // connection, so none is left unfinalized and `sqlite3_close` does not answer
        // SQLITE_BUSY; there is no caller to hand any other answer to.
        let _ = unsafe { ffi::sqlite3_close(self.db.as_ptr()) };
    }
}

/// A row of `pragma_table_list`.
struct DbTable {
    name: String,
    kind: String,
    strict: bool,
}

/// A row of `pragma_table_xinfo`.
struct DbColumn {
    name: String,
    declared: String,
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
                    column: String::from(column.name),
                    schema_at: at,
                    database_at,
                },
                None => SchemaDifference::MissingColumn { column: String::from(column.name) },
            });
        };
        if here.hidden {
            return Err(SchemaDifference::Generated { column: String::from(column.name) });
        }
        if !declares(column.storage, &here.declared) {
            return Err(SchemaDifference::Storage {
                column: String::from(column.name),
                expected: column.storage,
                declared: here.declared.clone(),
            });
        }
        if here.not_null == column.nullable {
            return Err(SchemaDifference::Nullability {
                column: String::from(column.name),
                nullable_in_schema: column.nullable,
            });
        }
        if here.key != i64::from(schema_key) {
            return Err(SchemaDifference::PrimaryKey {
                column: String::from(column.name),
                schema: schema_key,
                database: here.key,
            });
        }
    }
    if let Some(extra) = found.get(table.columns.len()) {
        return Err(SchemaDifference::ExtraColumn { column: extra.name.clone() });
    }
    Ok(())
}

/// A STRICT table's declared type stores `storage`: `int` or `integer` for an integer (and a
/// Boolean, kept as an integer), else the storage's own name.
fn declares(storage: Storage, declared: &str) -> bool {
    match storage {
        Storage::Boolean | Storage::Integer => {
            declared.eq_ignore_ascii_case("integer") || declared.eq_ignore_ascii_case("int")
        }
        Storage::Real => declared.eq_ignore_ascii_case("real"),
        Storage::Text => declared.eq_ignore_ascii_case("text"),
        Storage::Blob => declared.eq_ignore_ascii_case("blob"),
    }
}

impl<'c> Raw<'c> {
    /// Prepares `text`, which must be exactly one statement.
    fn prepare(connection: &'c Connection, text: String, flags: c_uint) -> Result<Raw<'c>, Error> {
        let Ok(length) = c_int::try_from(text.len()) else {
            let bytes = text.len();
            return Err(Error::Prepare {
                statement: text,
                difference: PrepareDifference::TooLong { bytes },
            });
        };
        let start = text.as_ptr().cast::<c_char>();
        let mut stmt: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        // SAFETY: the connection is open; `start` points at `length` bytes of `text`, which
        // outlives the call (SQLite reads no further than `length`, nor past a NUL); `stmt`
        // and `tail` are valid places for SQLite's answers.
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
            return Err(Error::Prepare {
                statement: text,
                difference: PrepareDifference::NoStatement,
            });
        };
        // From here the statement is finalized on drop.
        let raw = Raw { stmt, connection, text };
        // `tail` points into `text`, just past the first statement; compared as addresses.
        let used = (tail as usize).checked_sub(start as usize);
        let rest = used.and_then(|used| raw.text.get(used..));
        match rest {
            Some("") => Ok(raw),
            Some(rest) => Err(Error::Prepare {
                statement: raw.text.clone(),
                difference: PrepareDifference::Tail { rest: String::from(rest) },
            }),
            None => Err(Error::Prepare {
                statement: raw.text.clone(),
                difference: PrepareDifference::Tail {
                    rest: String::from("(SQLite's tail is outside the text)"),
                },
            }),
        }
    }

    /// One step: `true` on a row, `false` when done.
    fn step(&mut self) -> Result<bool, Error> {
        // SAFETY: `self.stmt` is a live prepared statement, stepped by one thread (the
        // connection is not `Sync`).
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
        // SAFETY: `stmt` is live and not mid-step (the callers bind only before the first
        // step); SQLite answers SQLITE_RANGE for a `k` outside its placeholders. A text's or a
        // blob's pointer is valid for its length for the call, never null (a Rust slice's
        // pointer is not, even when empty, so an empty one binds as empty, not as null), and
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
                    text.len() as u64,
                    ffi::SQLITE_TRANSIENT(),
                    ffi::SQLITE_UTF8 as u8,
                ),
                ValueRef::Blob(bytes) => ffi::sqlite3_bind_blob64(
                    stmt,
                    k,
                    bytes.as_ptr().cast(),
                    bytes.len() as u64,
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
    fn value(&self, index: c_int) -> Result<RawValue<'_>, Error> {
        let stmt = self.stmt.as_ptr();
        // SAFETY: `stmt` is live and on a row, and `index` is below its column count (both
        // the callers').
        let class = unsafe { ffi::sqlite3_column_type(stmt, index) };
        match class {
            ffi::SQLITE_NULL => Ok(RawValue::Null),
            // SAFETY: as above; the value is an integer, read as one.
            ffi::SQLITE_INTEGER => {
                Ok(RawValue::Integer(unsafe { ffi::sqlite3_column_int64(stmt, index) }))
            }
            // SAFETY: as above; the value is a real, read as one.
            ffi::SQLITE_FLOAT => {
                Ok(RawValue::Real(unsafe { ffi::sqlite3_column_double(stmt, index) }))
            }
            ffi::SQLITE_TEXT => {
                // SAFETY: as above; the value is text, read as text, then its length in bytes
                // (in that order, as SQLite documents).
                let (data, length) = unsafe {
                    let data = ffi::sqlite3_column_text(stmt, index);
                    (data.cast::<u8>(), ffi::sqlite3_column_bytes(stmt, index))
                };
                Ok(RawValue::Text(self.bytes(data, length, "sqlite3_column_text")?))
            }
            ffi::SQLITE_BLOB => {
                // SAFETY: as above; the value is a blob, read as one, then its length.
                let (data, length) = unsafe {
                    let data = ffi::sqlite3_column_blob(stmt, index);
                    (data.cast::<u8>(), ffi::sqlite3_column_bytes(stmt, index))
                };
                Ok(RawValue::Blob(self.bytes(data, length, "sqlite3_column_blob")?))
            }
            class => Err(Error::StorageClass {
                statement: self.text.clone(),
                index: u32::try_from(index).unwrap_or(0),
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
        // SAFETY: SQLite keeps these `length` bytes valid until the statement steps, resets or
        // is finalized, or the column is read as another type. Stepping and resetting take
        // `&mut` of the statement and finalizing is its drop, so none happens while the `&self`
        // this slice borrows lives; and a column is only ever read in its own storage class
        // (text as text, a blob as a blob), so it is never converted.
        Ok(unsafe { core::slice::from_raw_parts(data, length) })
    }

    /// Column `index` of the current row, which must be text. For the driver's own queries.
    fn text_at(&self, index: c_int) -> Result<&str, Error> {
        match self.value(index)? {
            RawValue::Text(bytes) => core::str::from_utf8(bytes).map_err(|_| Error::NotUtf8 {
                statement: self.text.clone(),
                column: String::new(),
                index: u32::try_from(index).unwrap_or(0),
            }),
            other => Err(self.internal_type(index, Storage::Text, other.found())),
        }
    }

    /// Column `index` of the current row, which must be an integer. For the driver's own
    /// queries.
    fn integer_at(&self, index: c_int) -> Result<i64, Error> {
        match self.value(index)? {
            RawValue::Integer(value) => Ok(value),
            other => Err(self.internal_type(index, Storage::Integer, other.found())),
        }
    }

    fn internal_type(&self, index: c_int, storage: Storage, got: Found) -> Error {
        Error::ColumnType {
            statement: self.text.clone(),
            column: String::new(),
            index: u32::try_from(index).unwrap_or(0),
            expected: Expected { storage, nullable: false },
            got,
        }
    }
}

impl Drop for Raw<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.stmt` is live and finalized only here. Its answer repeats the last
        // step's failure, which that step already returned.
        let _ = unsafe { ffi::sqlite3_finalize(self.stmt.as_ptr()) };
    }
}

impl Statement<'_> {
    /// What binds to each placeholder: `slots()[k - 1]` to placeholder `k`.
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// Binds placeholder `k` (from 1). Text and blobs are copied by SQLite (SQLITE_TRANSIENT).
    /// The value must be of the parameter's storage, null only where it is nullable, and not a
    /// NaN; only before the first step (after [`Statement::reset`]).
    pub fn bind(&mut self, k: u32, value: ValueRef<'_>) -> Result<(), Error> {
        if self.state != State::Ready {
            return Err(self.state_error("bind"));
        }
        let no_placeholder = || Error::NoPlaceholder {
            statement: self.raw.text.clone(),
            placeholder: k,
            count: count_u32(self.slots.len()),
        };
        let Some(at) = (k as usize).checked_sub(1) else {
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
                return Err(Error::Prepare {
                    statement: self.raw.text.clone(),
                    difference: PrepareDifference::ArraySlot { placeholder: k },
                });
            }
        };
        let Some(shape) = self.params.get(param as usize) else {
            return Err(Error::Prepare {
                statement: self.raw.text.clone(),
                difference: PrepareDifference::SlotParam {
                    placeholder: k,
                    param,
                    params: count_u32(self.params.len()),
                },
            });
        };
        let admitted = match value.storage() {
            None => shape.expected.nullable,
            Some(storage) => storage == shape.expected.storage,
        };
        if !admitted {
            return Err(Error::BindType {
                statement: self.raw.text.clone(),
                placeholder: k,
                param: shape.name.clone(),
                expected: shape.expected,
                got: value.storage(),
            });
        }
        if let ValueRef::Real(real) = value
            && real.is_nan()
        {
            return Err(Error::BindNan {
                statement: self.raw.text.clone(),
                placeholder: k,
                param: shape.name.clone(),
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
                        ) => self.params.get(*param as usize).map(|shape| shape.name.clone()),
                        None => None,
                    };
                    return Err(Error::Unbound {
                        statement: self.raw.text.clone(),
                        placeholder: count_u32(at + 1),
                        param: param.unwrap_or_default(),
                    });
                }
            }
            State::Row => {}
            State::Done | State::Failed => return Err(self.state_error("step")),
        }
        match self.raw.step() {
            Ok(true) => {
                self.state = State::Row;
                Ok(Step::Row)
            }
            Ok(false) => {
                self.state = State::Done;
                let stmt = self.raw.stmt.as_ptr();
                // SAFETY: `stmt` is a live prepared statement.
                if unsafe { ffi::sqlite3_stmt_readonly(stmt) } != 0 {
                    return Ok(Step::Done { changed: 0 });
                }
                // SAFETY: the connection is open; the count is of the statement that just
                // finished, as nothing else ran on this connection in between (it is not
                // `Sync`, and this is one call).
                let count = unsafe { ffi::sqlite3_changes64(self.raw.connection.db.as_ptr()) };
                let changed = u64::try_from(count)
                    .map_err(|_| Error::Changes { statement: self.raw.text.clone(), count })?;
                Ok(Step::Done { changed })
            }
            Err(error) => {
                self.state = State::Failed;
                Err(error)
            }
        }
    }

    /// Column `index` (from 0) of the current row, checked against the description: null only
    /// where nullable; the storage class the description names (a Boolean is an integer 0 or
    /// 1); text that is UTF-8.
    pub fn column(&self, index: u32) -> Result<ValueRef<'_>, Error> {
        if self.state != State::Row {
            return Err(self.state_error("column"));
        }
        let no_column = || Error::NoColumn {
            statement: self.raw.text.clone(),
            index,
            width: count_u32(self.columns.len()),
        };
        let Some(shape) = self.columns.get(index as usize) else {
            return Err(no_column());
        };
        let Ok(index_c) = c_int::try_from(index) else {
            return Err(no_column());
        };
        let value = self.raw.value(index_c)?;
        let expected = shape.expected;
        match (value, expected.storage) {
            (RawValue::Null, _) if expected.nullable => Ok(ValueRef::Null),
            (RawValue::Integer(value), Storage::Integer) => Ok(ValueRef::Integer(value)),
            (RawValue::Integer(0), Storage::Boolean) => Ok(ValueRef::Boolean(false)),
            (RawValue::Integer(1), Storage::Boolean) => Ok(ValueRef::Boolean(true)),
            (RawValue::Real(value), Storage::Real) => Ok(ValueRef::Real(value)),
            (RawValue::Text(bytes), Storage::Text) => match core::str::from_utf8(bytes) {
                Ok(text) => Ok(ValueRef::Text(text)),
                Err(_) => Err(Error::NotUtf8 {
                    statement: self.raw.text.clone(),
                    column: shape.name.clone(),
                    index,
                }),
            },
            (RawValue::Blob(bytes), Storage::Blob) => Ok(ValueRef::Blob(bytes)),
            (other, _) => Err(Error::ColumnType {
                statement: self.raw.text.clone(),
                column: shape.name.clone(),
                index,
                expected,
                got: other.found(),
            }),
        }
    }

    /// Back to before the first step, binds cleared. After a failed step, SQLite's reset
    /// answers that step's failure again, which the step already returned; it is not returned
    /// twice.
    pub fn reset(&mut self) -> Result<(), Error> {
        let stmt = self.raw.stmt.as_ptr();
        let failed = self.state == State::Failed;
        // SAFETY: `stmt` is a live prepared statement; a reset is allowed in any state.
        let code = unsafe { ffi::sqlite3_reset(stmt) };
        // SAFETY: as above.
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
        Error::State { statement: self.raw.text.clone(), call, state: self.state.name() }
    }
}

/// `bytes` and a NUL after them, refusing a NUL among them.
fn nul_terminated(bytes: &[u8], what: &'static str, shown: &str) -> Result<Vec<u8, Global>, Error> {
    if let Some(at) = bytes.iter().position(|byte| *byte == 0) {
        return Err(Error::Nul { what, text: String::from(shown), at });
    }
    let mut text = Vec::with_capacity_in(bytes.len() + 1, Global);
    text.extend_from_slice(bytes);
    text.push(0);
    Ok(text)
}

/// The bytes of the NUL-terminated string at `text`, without the NUL; `None` for null.
///
/// # Safety
/// `text` is null or points at a NUL-terminated string that stays valid and unchanged for `'a`.
unsafe fn c_text<'a>(text: *const c_char) -> Option<&'a [u8]> {
    if text.is_null() {
        return None;
    }
    // SAFETY: `text` is not null, and the caller promises the rest.
    Some(unsafe { CStr::from_ptr(text) }.to_bytes())
}

/// SQLite's English name for result code `code`.
fn code_text(code: c_int) -> &'static str {
    // SAFETY: `sqlite3_errstr` takes any code and answers a pointer into SQLite's static table
    // of NUL-terminated ASCII names, valid for the program's life.
    let text = unsafe { c_text::<'static>(ffi::sqlite3_errstr(code)) };
    text.and_then(|text| core::str::from_utf8(text).ok()).unwrap_or("unknown result code")
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
        let Some(next) =
            value.checked_mul(10).and_then(|value| value.checked_add(usize::from(digit - b'0')))
        else {
            return false;
        };
        value = next;
    }
    value == k
}

/// A count or a position as `u32`, for messages; SQLite's own counts are `int`s, so the ones
/// the driver holds fit.
fn count_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn display(value: &dyn fmt::Display) -> String {
    let mut text = String::new();
    // Writing into a `String` does not fail.
    let _ = fmt::Write::write_fmt(&mut text, format_args!("{value}"));
    text
}

fn storage_name(storage: Storage) -> &'static str {
    match storage {
        Storage::Boolean => "a boolean (an integer 0 or 1)",
        Storage::Integer => "an integer",
        Storage::Real => "a real",
        Storage::Text => "text",
        Storage::Blob => "a blob",
    }
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(storage_name(self.storage))?;
        if self.nullable {
            f.write_str(" or null")?;
        }
        Ok(())
    }
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Found::Null => f.write_str("null"),
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
            PrepareDifference::NotUtf8 => f.write_str("the rendered text is not UTF-8"),
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
            SchemaDifference::Storage { column, expected, declared } => write!(
                f,
                "column `{column}` is declared `{declared}` in the database, but the schema stores {}",
                storage_name(*expected)
            ),
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

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite { call, code, code_text, message, statement } => write!(
                f,
                "SQLite answered `{call}` with {code} ({code_text}): {message}; statement: `{statement}`"
            ),
            Error::Open { path, code, code_text, message } => {
                write!(f, "SQLite did not open `{path}`: {code} ({code_text}): {message}")
            }
            Error::Pragma { path, pragma, what } => {
                write!(f, "`{pragma}` did not take on the connection to `{path}`: {what}")
            }
            Error::Nul { what, text, at } => {
                write!(f, "{what} holds a NUL byte at {at}, which a C string cannot: `{text}`")
            }
            Error::Render(message) => write!(f, "no SQLite text for the statement: {message}"),
            Error::Ddl { table } => write!(f, "writing `create table` for `{table}` failed"),
            Error::Rollback { create, rollback } => write!(
                f,
                "creating the tables failed ({create}), and rolling back failed too ({rollback})"
            ),
            Error::Prepare { statement, difference } => {
                write!(
                    f,
                    "SQLite's account of `{statement}` differs from the checker's: {difference}"
                )
            }
            Error::Schema { table, difference } => {
                write!(f, "table `{table}` differs from the schema: {difference}")
            }
            Error::ColumnType { statement, column, index, expected, got } => write!(
                f,
                "column `{column}` (at {index}) of `{statement}` held {got}, but the description admits {expected}"
            ),
            Error::NotUtf8 { statement, column, index } => write!(
                f,
                "column `{column}` (at {index}) of `{statement}` holds text that is not UTF-8"
            ),
            Error::StorageClass { statement, index, class } => write!(
                f,
                "SQLite gave column {index} of `{statement}` the undocumented storage class {class}"
            ),
            Error::BindType { statement, placeholder, param, expected, got } => {
                let got = match got {
                    Some(storage) => storage_name(*storage),
                    None => "null",
                };
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
            Error::Changes { statement, count } => {
                write!(f, "SQLite counted {count} changed rows for `{statement}`")
            }
        }
    }
}

impl std::error::Error for Error {}
