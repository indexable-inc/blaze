//! No injection: whatever bytes a parameter carries, the statement SQLite runs is the one
//! written. Generated text and blob values (quotes, `;`, comment markers, backslashes, NUL,
//! classic injection strings, arbitrary unicode) go through checked `insert`, `update .. where`,
//! `select .. where name = $v` and `delete .. where` statements beside a model that treats each
//! value as data, and after every statement the table is the model's, still has its schema, and
//! every value read back is byte-equal to the one bound.
//!
//! The text side is closed by `check`: it refuses a second statement, a comment, and every
//! positional parameter form, so no such text reaches `prepare`, which takes only a `Checked`.

use std::alloc::Global;
use std::fmt::Write as _;

use blaze_sim::{Broke, Config, Tape};
use blaze_sql::desc::{Param, Slot, Storage, ValueRef};
use blaze_sql::schema::{Column, Schema, Table};

use crate::{Connection, Statement, Step};

const INSERT: &str = "insert into note (id, name, data) values ($id, $name, $data)";
const UPDATE: &str = "update note set name = $name, data = $data where name = $find";
const FIND: &str = "select id, name, data from note where name = $find order by id";
const DELETE: &str = "delete from note where name = $find";
const ALL: &str = "select id, name, data from note order by id";

/// Text that would change a statement were it spliced into one.
const HOSTILE_TEXT: [&str; 26] = [
    "",
    "'",
    "''",
    "'''",
    "\"",
    ";",
    "--",
    "/*",
    "*/",
    "\\",
    "\\'",
    "\0",
    "a\0b",
    "' or 1=1 --",
    "' or '1'='1",
    "'); drop table note; --",
    "\"); drop table note; --",
    "x'00'",
    "$name",
    "?1",
    "%",
    "_",
    "' || '",
    "🦀'; select 1; --",
    "\u{2028}\u{feff}",
    "\r\n'\t",
];

const HOSTILE_BYTES: [&[u8]; 8] =
    [b"'", b"\0", b"'); drop table note; --", b"--", b"/*", b"\\", b"\xff\xfe\x00", b"x'00'"];

const MAX_STEPS: u32 = 4096;
const CONTROL_CASES: u32 = 100;

/// `note (id integer primary key, name text not null, data blob not null)`.
fn note_columns() -> [Column<'static>; 3] {
    [
        Column {
            name: "id",
            storage: Storage::Integer,
            rust: Storage::Integer.plain(),
            nullable: false,
            primary_key: true,
        },
        Column {
            name: "name",
            storage: Storage::Text,
            rust: Storage::Text.plain(),
            nullable: false,
            primary_key: false,
        },
        Column {
            name: "data",
            storage: Storage::Blob,
            rust: Storage::Blob.plain(),
            nullable: false,
            primary_key: false,
        },
    ]
}

fn with_schema<R>(f: impl FnOnce(&Schema<'_>) -> R) -> R {
    let columns = note_columns();
    let table = Table { name: "note", columns: &columns };
    let tables = [&table];
    f(&Schema { tables: &tables })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    id: i64,
    name: String,
    data: Vec<u8>,
}

#[derive(Clone, Debug)]
enum Op {
    Insert { name: String, data: Vec<u8> },
    Update { find: String, name: String, data: Vec<u8> },
    Find { find: String },
    Delete { find: String },
}

/// How an `insert` puts its values in: bound to the checked statement's parameters, or (the
/// control's planted bug) spliced between quotes into text run as a script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Insertion {
    Bound,
    Spliced,
}

fn draw_text(tape: &Tape, earlier: &[String]) -> String {
    let roll = tape.int_in(0..=9u8, "text kind");
    if roll <= 2 && !earlier.is_empty() {
        return earlier[tape.index(earlier.len(), "earlier text")].clone();
    }
    if roll <= 6 {
        let mut text = (*tape.one_of(&HOSTILE_TEXT, "hostile text")).to_owned();
        if tape.chance(0.3, "second hostile text") {
            text.push_str(tape.one_of(&HOSTILE_TEXT, "hostile text"));
        }
        return text;
    }
    tape.text(0..=12, "text")
}

fn draw_blob(tape: &Tape) -> Vec<u8> {
    if tape.chance(0.5, "hostile blob") {
        tape.one_of(&HOSTILE_BYTES, "hostile blob").to_vec()
    } else {
        tape.bytes(0..=12, "blob")
    }
}

fn draw_ops(tape: &Tape) -> Vec<Op> {
    let count = tape.int_in(1..=12usize, "ops");
    let mut names: Vec<String> = Vec::new();
    let mut ops = Vec::new();
    for _ in 0..count {
        let op = tape.span("op", |tape| match tape.int_in(0..=9u8, "op") {
            0..=3 => Op::Insert { name: draw_text(tape, &names), data: draw_blob(tape) },
            4 | 5 => Op::Update {
                find: draw_text(tape, &names),
                name: draw_text(tape, &names),
                data: draw_blob(tape),
            },
            6 | 7 => Op::Find { find: draw_text(tape, &names) },
            _ => Op::Delete { find: draw_text(tape, &names) },
        });
        if let Op::Insert { name, .. } | Op::Update { name, .. } = &op {
            names.push(name.clone());
        }
        ops.push(op);
    }
    tape.note(format_args!("ops: {ops:?}"));
    ops
}

/// Binds each placeholder of `statement` by its parameter's name, from `named`; every name in
/// `named` must be a parameter of the statement.
fn bind_named(
    statement: &mut Statement<'_>,
    params: &[Param<'_>],
    named: &[(&str, ValueRef<'_>)],
    text: &str,
) -> Result<(), String> {
    for (name, _) in named {
        if !params.iter().any(|param| param.name == *name) {
            return Err(format!("{text}: the description has no parameter `${name}`"));
        }
    }
    let slots: Vec<Slot> = statement.slots().to_vec();
    for (at, slot) in slots.iter().enumerate() {
        let k = u32::try_from(at + 1).expect("fewer than 2^32 placeholders");
        let Slot::Scalar { param } = *slot else {
            return Err(format!(
                "{text}: placeholder {k} is {slot:?}, and the statement has no list"
            ));
        };
        let Some(param) = params.get(param as usize) else {
            return Err(format!(
                "{text}: placeholder {k} binds parameter {param}, past the description's"
            ));
        };
        let Some(&(_, value)) = named.iter().find(|(name, _)| *name == param.name) else {
            return Err(format!("{text}: no value for `${}`", param.name));
        };
        statement
            .bind(k, value)
            .map_err(|error| format!("{text}: binding `${}`: {error}", param.name))?;
    }
    Ok(())
}

/// Runs a statement that returns a count; the rows it changed.
fn execute(
    connection: &Connection,
    schema: &Schema<'_>,
    text: &'static str,
    named: &[(&str, ValueRef<'_>)],
) -> Result<u64, String> {
    let checked = blaze_sql::check(schema, text, Global)
        .map_err(|error| format!("{text}: refused: {error}"))?;
    let mut statement =
        connection.prepare(&checked, &[]).map_err(|error| format!("{text}: {error}"))?;
    bind_named(&mut statement, &checked.description().params, named, text)?;
    match statement.step().map_err(|error| format!("{text}: {error}"))? {
        Step::Done { changed } => Ok(changed),
        Step::Row => Err(format!("{text}: returned a row")),
    }
}

/// Runs a `select id, name, data`; its rows.
fn query(
    connection: &Connection,
    schema: &Schema<'_>,
    text: &'static str,
    named: &[(&str, ValueRef<'_>)],
) -> Result<Vec<Row>, String> {
    let checked = blaze_sql::check(schema, text, Global)
        .map_err(|error| format!("{text}: refused: {error}"))?;
    let mut statement =
        connection.prepare(&checked, &[]).map_err(|error| format!("{text}: {error}"))?;
    bind_named(&mut statement, &checked.description().params, named, text)?;
    let mut rows = Vec::new();
    for _ in 0..MAX_STEPS {
        match statement.step().map_err(|error| format!("{text}: {error}"))? {
            Step::Done { .. } => return Ok(rows),
            Step::Row => {
                let read = |index: u32| {
                    statement.column(index).map_err(|error| format!("{text}: {error}"))
                };
                let row = match (read(0)?, read(1)?, read(2)?) {
                    (ValueRef::Integer(id), ValueRef::Text(name), ValueRef::Blob(data)) => {
                        Row { id, name: name.to_owned(), data: data.to_vec() }
                    }
                    other => return Err(format!("{text}: a row of {other:?}")),
                };
                rows.push(row);
            }
        }
    }
    Err(format!("{text}: more than {MAX_STEPS} rows"))
}

/// The control's planted bug: the insert written with its values spliced into the text, the
/// name between quotes as it is, and run as a script.
fn spliced_insert(id: i64, name: &str, data: &[u8]) -> String {
    let mut text = format!("insert into note (id, name, data) values ({id}, '{name}', x'");
    for byte in data {
        let _ = write!(text, "{byte:02x}");
    }
    text.push_str("')");
    text
}

/// Runs `ops` on a fresh database and on the model, comparing after every one.
fn runs_as_the_model_does(ops: &[Op], insertion: Insertion) -> Result<(), String> {
    with_schema(|schema| {
        let connection = Connection::open_memory().map_err(|error| format!("open: {error}"))?;
        connection.create(schema).map_err(|error| format!("create: {error}"))?;
        let mut model: Vec<Row> = Vec::new();
        let mut next_id: i64 = 1;
        for (at, op) in ops.iter().enumerate() {
            let (changed, expected) = match op {
                Op::Insert { name, data } => {
                    let changed = match insertion {
                        Insertion::Bound => execute(
                            &connection,
                            schema,
                            INSERT,
                            &[
                                ("id", ValueRef::Integer(next_id)),
                                ("name", ValueRef::Text(name)),
                                ("data", ValueRef::Blob(data)),
                            ],
                        )?,
                        Insertion::Spliced => {
                            connection
                                .execute_script(&spliced_insert(next_id, name, data))
                                .map_err(|error| {
                                    format!("op {at}: the spliced insert failed: {error}")
                                })?;
                            1
                        }
                    };
                    model.push(Row { id: next_id, name: name.clone(), data: data.clone() });
                    next_id += 1;
                    (changed, 1)
                }
                Op::Update { find, name, data } => {
                    let changed = execute(
                        &connection,
                        schema,
                        UPDATE,
                        &[
                            ("name", ValueRef::Text(name)),
                            ("data", ValueRef::Blob(data)),
                            ("find", ValueRef::Text(find)),
                        ],
                    )?;
                    let mut hit = 0u64;
                    for row in &mut model {
                        if row.name == *find {
                            row.name = name.clone();
                            row.data = data.clone();
                            hit += 1;
                        }
                    }
                    (changed, hit)
                }
                Op::Find { find } => {
                    let rows = query(&connection, schema, FIND, &[("find", ValueRef::Text(find))])?;
                    let want: Vec<Row> =
                        model.iter().filter(|row| row.name == *find).cloned().collect();
                    if rows != want {
                        return Err(format!(
                            "op {at}: `name = $find` with {find:?} found {rows:?}; the model holds {want:?}"
                        ));
                    }
                    if let Some(row) =
                        rows.iter().find(|row| row.name.as_bytes() != find.as_bytes())
                    {
                        return Err(format!("op {at}: {find:?} read back as {:?}", row.name));
                    }
                    (0, 0)
                }
                Op::Delete { find } => {
                    let changed =
                        execute(&connection, schema, DELETE, &[("find", ValueRef::Text(find))])?;
                    let before = model.len();
                    model.retain(|row| row.name != *find);
                    (changed, u64::try_from(before - model.len()).expect("a small count"))
                }
            };
            if changed != expected {
                return Err(format!(
                    "op {at} ({op:?}) changed {changed} rows; the model changed {expected}"
                ));
            }
            connection.verify(schema).map_err(|error| {
                format!("after op {at} ({op:?}) the database's schema differs: {error}")
            })?;
            let rows = query(&connection, schema, ALL, &[])?;
            if rows != model {
                return Err(format!(
                    "after op {at} ({op:?}) the table holds {rows:?}; the model holds {model:?}"
                ));
            }
        }
        Ok(())
    })
}

/// The invariant: whatever bytes a text or blob parameter carries, the statement executed is
/// the one written: after every insert, update, select and delete the table's rows equal a
/// model's that treats each value as data, the table still exists with its schema, and each
/// value read back is byte-equal to the one bound.
#[test]
fn a_parameter_is_data_whatever_its_bytes() {
    blaze_sim::check(Config::cases(300), |tape| {
        runs_as_the_model_does(&draw_ops(tape), Insertion::Bound)
    });
}

/// An insert that splices its value between quotes into the text runs another statement for
/// some drawn value (it errors, or the table leaves the model); the oracle must see it.
#[test]
fn the_oracle_catches_a_value_spliced_into_the_text() {
    let directed = [Op::Insert { name: "'); drop table note; --".to_owned(), data: b"x".to_vec() }];
    assert_eq!(
        runs_as_the_model_does(&directed, Insertion::Bound),
        Ok(()),
        "bound, the value is data"
    );
    assert!(
        runs_as_the_model_does(&directed, Insertion::Spliced).is_err(),
        "the oracle missed a value spliced into the text"
    );

    // Each case runs bound first, which must pass, then spliced with the same ops; the
    // control passes once the oracle refuses a spliced run, and panics naming the plant when
    // no drawn value in `CONTROL_CASES` cases made the spliced insert diverge.
    blaze_sim::control(
        Config::cases(CONTROL_CASES),
        Insertion::Bound,
        Insertion::Spliced,
        "",
        |tape, insertion| {
            let ops = draw_ops(tape);
            runs_as_the_model_does(&ops, Insertion::Bound)
                .map_err(|why| Broke::Harness(format!("bound: {why}")))?;
            runs_as_the_model_does(&ops, insertion).map_err(Broke::Oracle)
        },
    );
}

/// `check` refuses `text`, by syntax, with a message holding `token`.
fn refused(text: &'static str, token: &str) {
    with_schema(|schema| match blaze_sql::check(schema, text, Global) {
        Ok(_) => panic!("`{text}` was accepted"),
        Err(error) => {
            assert!(
                matches!(error, blaze_sql::Error::Syntax(_)),
                "`{text}`: refused by type: {error}"
            );
            let message = error.to_string();
            assert!(
                message.contains(token),
                "`{text}`: the message does not name `{token}`: {message}"
            );
        }
    });
}

#[test]
fn check_refuses_a_second_statement() {
    refused("select id from note where name = $v; drop table note", "second statement");
    refused("delete from note where name = $v; delete from note", "second statement");
    refused("select id from note;;", "second statement");
    with_schema(|schema| {
        let one = blaze_sql::check(schema, "select id from note where name = $v;", Global);
        assert!(
            one.is_ok(),
            "one statement and its `;`: {:?}",
            one.err().map(|error| error.to_string())
        );
    });
}

#[test]
fn check_refuses_a_comment() {
    refused("select id from note -- where name = $v", "--");
    refused("select id from note where name = $v --", "--");
    refused("select id /* , data */ from note", "/*");
}

#[test]
fn check_refuses_every_positional_parameter() {
    refused("select id from note where name = ?", "?");
    refused("select id from note where name = ?1", "?1");
    refused("select id from note where name = $1", "$1");
    refused("select id from note where name = :name", ":name");
    refused("select id from note where name = @name", "@name");
}
