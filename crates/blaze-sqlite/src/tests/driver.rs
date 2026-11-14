//! The driver against real SQLite: `create` then `verify`, each schema difference `verify`
//! names, values of every storage and null through `check`, `prepare`, `bind`, `step` and
//! `column`, the refusals of `bind`, `step` and `column`, and the `create table` text.

use core::any::TypeId;
use std::alloc::Global;

use blaze_sql::Checked;
use blaze_sql::ddl::create_table;
use blaze_sql::desc::{Backend, RustTy, Slot, Storage, ValueRef};
use blaze_sql::schema::{Column, Schema, Table};
use blaze_sim::{Config, Tape};

use crate::{
    ColumnTypeError, Connection, DeclaredType, Error, Found, SchemaDifference, SchemaError,
    Statement, Step,
};

const BOOL: RustTy<'static> = RustTy { name: "bool", id: TypeId::of::<bool>() };
const I64: RustTy<'static> = RustTy { name: "i64", id: TypeId::of::<i64>() };
const F64: RustTy<'static> = RustTy { name: "f64", id: TypeId::of::<f64>() };
const STRING: RustTy<'static> = RustTy { name: "String", id: TypeId::of::<String>() };
const BYTES: RustTy<'static> = RustTy { name: "Vec<u8>", id: TypeId::of::<Vec<u8>>() };

const fn column(
    name: &'static str,
    storage: Storage,
    rust: RustTy<'static>,
    nullable: bool,
    primary_key: bool,
) -> Column<'static> {
    Column { name, storage, rust, nullable, primary_key }
}

/// One column of each storage, none nullable.
static KINDS_COLUMNS: [Column<'static>; 6] = [
    column("id", Storage::Integer, I64, false, true),
    column("flag", Storage::Boolean, BOOL, false, false),
    column("amount", Storage::Integer, I64, false, false),
    column("ratio", Storage::Real, F64, false, false),
    column("label", Storage::Text, STRING, false, false),
    column("data", Storage::Blob, BYTES, false, false),
];
/// The same storages, each nullable.
static MAYBE_COLUMNS: [Column<'static>; 6] = [
    column("id", Storage::Integer, I64, false, true),
    column("flag", Storage::Boolean, BOOL, true, false),
    column("amount", Storage::Integer, I64, true, false),
    column("ratio", Storage::Real, F64, true, false),
    column("label", Storage::Text, STRING, true, false),
    column("data", Storage::Blob, BYTES, true, false),
];
static KINDS: Table<'static> = Table { name: "kinds", columns: &KINDS_COLUMNS };
static MAYBE: Table<'static> = Table { name: "maybe", columns: &MAYBE_COLUMNS };
static TABLES: [&Table<'static>; 2] = [&KINDS, &MAYBE];
static SCHEMA: Schema<'static> = Schema { tables: &TABLES };

/// Two primary-key columns, and a name with a `"` in it.
static PAIR_COLUMNS: [Column<'static>; 3] = [
    column("a", Storage::Integer, I64, false, true),
    column("b", Storage::Text, STRING, false, true),
    column("say \"hi\"", Storage::Real, F64, true, false),
];
static PAIR: Table<'static> = Table { name: "pair", columns: &PAIR_COLUMNS };
static PAIR_TABLES: [&Table<'static>; 1] = [&PAIR];
static PAIR_SCHEMA: Schema<'static> = Schema { tables: &PAIR_TABLES };

/// An integer column, for a table made without STRICT.
static LOOSE_COLUMNS: [Column<'static>; 1] = [column("n", Storage::Integer, I64, false, false)];
static LOOSE: Table<'static> = Table { name: "loose", columns: &LOOSE_COLUMNS };
static LOOSE_TABLES: [&Table<'static>; 1] = [&LOOSE];
static LOOSE_SCHEMA: Schema<'static> = Schema { tables: &LOOSE_TABLES };

const INSERT_KINDS: &str = "insert into kinds (id, flag, amount, ratio, label, data) \
    values ($id, $flag, $amount, $ratio, $label, $data)";
const SELECT_KINDS: &str = "select id, flag, amount, ratio, label, data from kinds where id = $id";
const INSERT_MAYBE: &str = "insert into maybe (id, flag, amount, ratio, label, data) \
    values ($id, $flag, $amount, $ratio, $label, $data)";
const SELECT_MAYBE: &str = "select id, flag, amount, ratio, label, data from maybe where id = $id";

fn ok<T>(result: Result<T, Error>) -> T {
    result.unwrap_or_else(|error| panic!("{error}"))
}

fn checked(schema: &Schema<'static>, text: &'static str) -> Checked<'static, Global> {
    blaze_sql::check(schema, text, Global).unwrap_or_else(|error| panic!("check `{text}`: {error}"))
}

fn ddl(table: &Table<'_>) -> String {
    let mut text = String::new();
    create_table(table, Backend::Sqlite, &mut text)
        .unwrap_or_else(|_| panic!("ddl for {}", table.name));
    text
}

/// A connection with `schema`'s tables created.
fn created(schema: &Schema<'_>) -> Connection {
    let connection = ok(Connection::open_memory());
    ok(connection.create(schema));
    connection
}

/// The placeholder that binds the parameter named `name`.
fn placeholder(statement: &Statement<'_>, checked: &Checked<'_, Global>, name: &str) -> u32 {
    let params = &checked.description().params;
    let at = statement
        .slots()
        .iter()
        .position(|slot| match slot {
            Slot::Scalar { param } => params[*param as usize].name == name,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no placeholder binds ${name}"));
    at as u32 + 1
}

/// Binds every placeholder from `values`, by its parameter's name.
fn bind_all(
    statement: &mut Statement<'_>,
    checked: &Checked<'_, Global>,
    values: &[(&str, ValueRef<'_>)],
) {
    let params = &checked.description().params;
    let slots: Vec<Slot> = statement.slots().to_vec();
    for (at, slot) in slots.iter().enumerate() {
        let Slot::Scalar { param } = slot else {
            panic!("placeholder {} is not a scalar: {slot:?}", at + 1);
        };
        let name = params[*param as usize].name;
        let value = values
            .iter()
            .find(|(given, _)| *given == name)
            .map(|(_, value)| *value)
            .unwrap_or_else(|| panic!("no value given for ${name}"));
        ok(statement.bind(at as u32 + 1, value));
    }
}

/// An owned copy of a value read, to keep past the row.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<ValueRef<'_>> for Value {
    fn from(value: ValueRef<'_>) -> Value {
        match value {
            ValueRef::Null => Value::Null,
            ValueRef::Boolean(value) => Value::Boolean(value),
            ValueRef::Integer(value) => Value::Integer(value),
            ValueRef::Real(value) => Value::Real(value),
            ValueRef::Text(text) => Value::Text(text.into()),
            ValueRef::Blob(bytes) => Value::Blob(bytes.into()),
        }
    }
}

/// Every row `statement` gives, `width` columns each.
fn rows(statement: &mut Statement<'_>, width: u32) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    loop {
        match ok(statement.step()) {
            Step::Row => rows
                .push((0..width).map(|index| Value::from(ok(statement.column(index)))).collect()),
            Step::Done { .. } => return rows,
        }
    }
}

/// Runs `checked` with `values` bound to its end; `changed` of its `Done`.
fn run(
    connection: &Connection,
    checked: &Checked<'_, Global>,
    values: &[(&str, ValueRef<'_>)],
) -> u64 {
    let mut statement = ok(connection.prepare(checked, &[]));
    bind_all(&mut statement, checked, values);
    loop {
        match ok(statement.step()) {
            Step::Row => {}
            Step::Done { changed } => return changed,
        }
    }
}

#[test]
fn create_then_verify_passes() {
    ok(created(&SCHEMA).verify(&SCHEMA));
    ok(created(&PAIR_SCHEMA).verify(&PAIR_SCHEMA));
}

/// `verify` of [`SCHEMA`] on a database made by `script`.
fn verify_after(script: &str) -> Error {
    let connection = ok(Connection::open_memory());
    ok(connection.execute_script(script));
    match connection.verify(&SCHEMA) {
        Ok(()) => panic!("verify passed a database made by `{script}`"),
        Err(error) => error,
    }
}

fn assert_schema_error(error: Error, table: &str, want: SchemaDifference) {
    let message = format!("{error}");
    match error {
        Error::Schema(SchemaError { table: got_table, difference }) => {
            assert_eq!(got_table, table, "{message}");
            assert_eq!(difference, want, "{message}");
            assert!(message.contains(table), "the message names the table: {message}");
        }
        other => panic!("not a schema difference: {other}"),
    }
}

#[test]
fn verify_names_a_missing_table() {
    let error = verify_after(&format!("{};", ddl(&KINDS)));
    assert_schema_error(error, "maybe", SchemaDifference::Missing);
}

#[test]
fn verify_names_a_missing_column() {
    let maybe = "create table \"maybe\" (\"id\" integer not null primary key, \
        \"flag\" integer check (\"flag\" in (0, 1)), \"amount\" integer, \"ratio\" real, \
        \"label\" text) strict";
    let error = verify_after(&format!("{}; {maybe};", ddl(&KINDS)));
    assert_schema_error(error, "maybe", SchemaDifference::MissingColumn { column: "data".into() });
}

#[test]
fn verify_names_a_column_of_another_storage() {
    let maybe = "create table \"maybe\" (\"id\" integer not null primary key, \
        \"flag\" integer check (\"flag\" in (0, 1)), \"amount\" integer, \"ratio\" text, \
        \"label\" text, \"data\" blob) strict";
    let error = verify_after(&format!("{}; {maybe};", ddl(&KINDS)));
    let message = format!("{error}");
    // SQLite may give the declared type in another case than it was written in.
    match error {
        Error::Schema(SchemaError {
            table,
            difference: SchemaDifference::Storage(DeclaredType { column, expected, declared }),
        }) => {
            assert_eq!(table, "maybe", "{message}");
            assert_eq!(column, "ratio", "{message}");
            assert_eq!(expected, Storage::Real, "{message}");
            assert!(declared.eq_ignore_ascii_case("text"), "{message}");
        }
        other => panic!("not a storage difference: {other}"),
    }
}

#[test]
fn like_is_case_sensitive() {
    let connection = created(&SCHEMA);
    for (text, want) in
        [("select 'A' like 'a' as matched", false), ("select 'a' like 'a' as matched", true)]
    {
        let select = checked(&SCHEMA, text);
        let mut statement = ok(connection.prepare(&select, &[]));
        assert_eq!(rows(&mut statement, 1), vec![vec![Value::Boolean(want)]], "{text}");
    }
}

#[test]
fn verify_names_a_nullable_column_declared_not_null() {
    let maybe = "create table \"maybe\" (\"id\" integer not null primary key, \
        \"flag\" integer check (\"flag\" in (0, 1)), \"amount\" integer, \"ratio\" real, \
        \"label\" text not null, \"data\" blob) strict";
    let error = verify_after(&format!("{}; {maybe};", ddl(&KINDS)));
    assert_schema_error(
        error,
        "maybe",
        SchemaDifference::Nullability { column: "label".into(), nullable_in_schema: true },
    );
}

#[test]
fn verify_names_a_table_that_is_not_strict() {
    let kinds = ddl(&KINDS);
    let loose = kinds.strip_suffix(" strict").unwrap_or_else(|| panic!("not strict: {kinds}"));
    let error = verify_after(&format!("{loose}; {};", ddl(&MAYBE)));
    assert_schema_error(error, "kinds", SchemaDifference::NotStrict);
}

#[test]
fn every_storage_and_null_round_trip() {
    let connection = created(&SCHEMA);
    let insert = checked(&SCHEMA, INSERT_KINDS);
    let values = [
        ("id", ValueRef::Integer(7)),
        ("flag", ValueRef::Boolean(true)),
        ("amount", ValueRef::Integer(-3)),
        ("ratio", ValueRef::Real(2.5)),
        ("label", ValueRef::Text("it's; -- \"quoted\"")),
        ("data", ValueRef::Blob(&[0, 1, 255])),
    ];
    assert_eq!(run(&connection, &insert, &values), 1);
    let select = checked(&SCHEMA, SELECT_KINDS);
    let mut statement = ok(connection.prepare(&select, &[]));
    bind_all(&mut statement, &select, &[("id", ValueRef::Integer(7))]);
    let want: Vec<Value> = values.iter().map(|(_, value)| Value::from(*value)).collect();
    assert_eq!(rows(&mut statement, 6), vec![want]);

    let insert = checked(&SCHEMA, INSERT_MAYBE);
    let nulls = [
        ("id", ValueRef::Integer(1)),
        ("flag", ValueRef::Null),
        ("amount", ValueRef::Null),
        ("ratio", ValueRef::Null),
        ("label", ValueRef::Null),
        ("data", ValueRef::Null),
    ];
    let full = [
        ("id", ValueRef::Integer(2)),
        ("flag", ValueRef::Boolean(false)),
        ("amount", ValueRef::Integer(i64::MIN)),
        ("ratio", ValueRef::Real(-1e300)),
        ("label", ValueRef::Text("")),
        ("data", ValueRef::Blob(&[])),
    ];
    assert_eq!(run(&connection, &insert, &nulls), 1);
    assert_eq!(run(&connection, &insert, &full), 1);
    let select = checked(&SCHEMA, SELECT_MAYBE);
    let mut statement = ok(connection.prepare(&select, &[]));
    for given in [&nulls, &full] {
        ok(statement.reset());
        bind_all(&mut statement, &select, &given[..1]);
        let want: Vec<Value> = given.iter().map(|(_, value)| Value::from(*value)).collect();
        assert_eq!(rows(&mut statement, 6), vec![want]);
    }
}

#[test]
fn column_refuses_text_in_an_integer_column_of_a_table_not_strict() {
    let connection = ok(Connection::open_memory());
    ok(connection.execute_script(
        "create table \"loose\" (\"n\" integer not null); insert into \"loose\" values ('abc');",
    ));
    let select = checked(&LOOSE_SCHEMA, "select n from loose");
    let mut statement = ok(connection.prepare(&select, &[]));
    assert_eq!(ok(statement.step()), Step::Row);
    let error =
        statement.column(0).err().unwrap_or_else(|| panic!("column read text as an integer"));
    let message = format!("{error}");
    match error {
        Error::ColumnType(ColumnTypeError { column, index, expected, got, .. }) => {
            assert_eq!(column, "n");
            assert_eq!(index, 0);
            assert_eq!(expected.storage, Storage::Integer);
            assert!(!expected.nullable);
            assert_eq!(got, Found::Text { bytes: 3 });
            assert!(message.contains("`n`"), "the message names the column: {message}");
        }
        other => panic!("not a column type error: {other}"),
    }
}

#[test]
fn done_counts_an_updates_rows() {
    let connection = created(&SCHEMA);
    let insert = checked(&SCHEMA, INSERT_KINDS);
    for id in 1..=3 {
        let row = [
            ("id", ValueRef::Integer(id)),
            ("flag", ValueRef::Boolean(false)),
            ("amount", ValueRef::Integer(0)),
            ("ratio", ValueRef::Real(0.5)),
            ("label", ValueRef::Text("x")),
            ("data", ValueRef::Blob(b"x")),
        ];
        assert_eq!(run(&connection, &insert, &row), 1);
    }
    let update = checked(&SCHEMA, "update kinds set amount = $amount where id <> $id");
    let changed = run(
        &connection,
        &update,
        &[("amount", ValueRef::Integer(9)), ("id", ValueRef::Integer(2))],
    );
    assert_eq!(changed, 2);
    let select = checked(&SCHEMA, "select id from kinds where id = $id");
    // A statement that writes nothing changed nothing, whatever ran before it.
    assert_eq!(run(&connection, &select, &[("id", ValueRef::Integer(1))]), 0);
}

#[test]
fn bind_refuses_a_value_of_another_storage() {
    let connection = created(&SCHEMA);
    let insert = checked(&SCHEMA, INSERT_KINDS);
    let mut statement = ok(connection.prepare(&insert, &[]));
    let k = placeholder(&statement, &insert, "amount");
    let message = match statement.bind(k, ValueRef::Text("7")) {
        Err(error @ Error::BindType { .. }) => format!("{error}"),
        other => panic!("bound text to an integer parameter: {other:?}"),
    };
    assert!(message.contains("$amount"), "the message names the parameter: {message}");
    let k = placeholder(&statement, &insert, "label");
    assert!(matches!(statement.bind(k, ValueRef::Null), Err(Error::BindType { .. })));
}

#[test]
fn bind_refuses_a_nan() {
    let connection = created(&SCHEMA);
    let insert = checked(&SCHEMA, INSERT_KINDS);
    let mut statement = ok(connection.prepare(&insert, &[]));
    let k = placeholder(&statement, &insert, "ratio");
    assert!(matches!(statement.bind(k, ValueRef::Real(f64::NAN)), Err(Error::BindNan { .. })));
}

#[test]
fn step_refuses_an_unbound_placeholder() {
    let connection = created(&SCHEMA);
    let select = checked(&SCHEMA, SELECT_KINDS);
    let mut statement = ok(connection.prepare(&select, &[]));
    match statement.step() {
        Err(Error::Unbound { param, placeholder: 1, .. }) => assert_eq!(param, "id"),
        other => panic!("stepped with $id unbound: {other:?}"),
    }
}

/// What the round trip writes: a value of every storage.
#[derive(Debug)]
struct Kinds {
    flag: bool,
    amount: i64,
    ratio: f64,
    label: String,
    data: Vec<u8>,
}

/// A bug to plant in the round trip, for the oracle's control.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Plant {
    /// The text bound loses its last character.
    TruncatedBind,
}

/// The oracle: `row` inserted into `kinds` and selected back reads equal (a real by `==`, so
/// `-0.0`, which SQLite may keep as the integer 0 of a REAL column, reads back as `0.0`).
fn reads_back(row: &Kinds, plant: Option<Plant>) -> Result<(), String> {
    let connection = created(&SCHEMA);
    let mut label = row.label.as_str();
    if plant == Some(Plant::TruncatedBind) {
        let mut chars = label.chars();
        chars.next_back();
        label = chars.as_str();
    }
    let values = [
        ("id", ValueRef::Integer(1)),
        ("flag", ValueRef::Boolean(row.flag)),
        ("amount", ValueRef::Integer(row.amount)),
        ("ratio", ValueRef::Real(row.ratio)),
        ("label", ValueRef::Text(label)),
        ("data", ValueRef::Blob(&row.data)),
    ];
    let insert = checked(&SCHEMA, INSERT_KINDS);
    assert_eq!(run(&connection, &insert, &values), 1);
    let select = checked(&SCHEMA, SELECT_KINDS);
    let mut statement = ok(connection.prepare(&select, &[]));
    bind_all(&mut statement, &select, &[("id", ValueRef::Integer(1))]);
    let got = rows(&mut statement, 6);
    let want = vec![vec![
        Value::Integer(1),
        Value::Boolean(row.flag),
        Value::Integer(row.amount),
        Value::Real(row.ratio),
        Value::Text(row.label.clone()),
        Value::Blob(row.data.clone()),
    ]];
    if got == want { Ok(()) } else { Err(format!("wrote {want:?}, read {got:?}")) }
}

fn kinds(tape: &Tape) -> Kinds {
    Kinds {
        flag: tape.bool("flag"),
        amount: tape.int("amount"),
        ratio: tape.f64_finite("ratio"),
        label: tape.text(0..=64, "label"),
        data: tape.bytes(0..=64, "data"),
    }
}

/// The invariant: any `i64`, finite `f64`, string (NUL and all) and byte string, bound and
/// stored in a STRICT table, reads back equal through `column`.
#[test]
fn any_value_binds_and_reads_back_equal() {
    blaze_sim::check(Config::DEFAULT, |tape| reads_back(&tape.span("row", kinds), None));
}

#[test]
fn the_oracle_catches_a_truncated_bind() {
    let row = Kinds { flag: true, amount: 1, ratio: 1.5, label: "ab".into(), data: vec![1] };
    assert!(reads_back(&row, None).is_ok());
    assert!(reads_back(&row, Some(Plant::TruncatedBind)).is_err());
}

#[test]
fn create_table_text_on_sqlite() {
    assert_eq!(
        ddl(&KINDS),
        "create table \"kinds\" (\"id\" integer not null primary key, \
         \"flag\" integer not null check (\"flag\" in (0, 1)), \"amount\" integer not null, \
         \"ratio\" real not null, \"label\" text not null, \"data\" blob not null) strict"
    );
    assert_eq!(
        ddl(&PAIR),
        "create table \"pair\" (\"a\" integer not null, \"b\" text not null, \
         \"say \"\"hi\"\"\" real, primary key (\"a\", \"b\")) strict"
    );
}

#[test]
fn create_table_text_on_postgres() {
    let postgres = |table: &Table<'_>| {
        let mut text = String::new();
        create_table(table, Backend::Postgres, &mut text).unwrap_or_else(|_| panic!("ddl"));
        text
    };
    assert_eq!(
        postgres(&KINDS),
        "create table \"kinds\" (\"id\" bigint generated by default as identity primary key, \
         \"flag\" boolean not null, \"amount\" bigint not null, \"ratio\" double precision not null, \
         \"label\" text not null, \"data\" bytea not null)"
    );
    assert_eq!(
        postgres(&PAIR),
        "create table \"pair\" (\"a\" bigint not null, \"b\" text not null, \
         \"say \"\"hi\"\"\" double precision, primary key (\"a\", \"b\"))"
    );
}
