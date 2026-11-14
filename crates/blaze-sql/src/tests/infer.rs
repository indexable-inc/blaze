//! Inference's typing rules, one test per rule (the accepted form with its exact description,
//! the refused form with its error), and rendering's exact text and slots for both engines.

extern crate std;

use alloc::alloc::Global;
use core::any::TypeId;
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

use blaze_sim::{Config, Tape};

use crate::desc::{Backend, Description, Param, ResultCol, Returns, RustTy, Slot, Storage, Ty};
use crate::infer::{Expected, Place, TypeError, TypeErrorKind, What, infer};
use crate::parse::parse;
use crate::render::{RenderError, render};
use crate::schema::{Column, Schema, Table};

#[allow(dead_code, reason = "only its TypeId is used, as a column's Rust type")]
struct PersonId;
#[allow(dead_code, reason = "only its TypeId is used, as a column's Rust type")]
struct OrderId;

const PERSON_ID: RustTy<'static> = RustTy { name: "PersonId", id: TypeId::of::<PersonId>() };
const ORDER_ID: RustTy<'static> = RustTy { name: "OrderId", id: TypeId::of::<OrderId>() };
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

static PERSON_COLUMNS: [Column<'static>; 5] = [
    column("id", Storage::Integer, PERSON_ID, false, true),
    column("name", Storage::Text, STRING, false, false),
    column("age", Storage::Integer, I64, false, false),
    column("city", Storage::Text, STRING, true, false),
    column("active", Storage::Boolean, BOOL, false, false),
];
static ORDERS_COLUMNS: [Column<'static>; 4] = [
    column("id", Storage::Integer, ORDER_ID, false, true),
    column("person", Storage::Integer, PERSON_ID, false, false),
    column("total", Storage::Real, F64, false, false),
    column("note", Storage::Text, STRING, true, false),
];
static BLOBS_COLUMNS: [Column<'static>; 2] = [
    column("id", Storage::Integer, I64, false, true),
    column("data", Storage::Blob, BYTES, false, false),
];
static PERSON: Table<'static> = Table { name: "person", columns: &PERSON_COLUMNS };
static ORDERS: Table<'static> = Table { name: "orders", columns: &ORDERS_COLUMNS };
static BLOBS: Table<'static> = Table { name: "blobs", columns: &BLOBS_COLUMNS };
static TABLES: [&Table<'static>; 3] = [&PERSON, &ORDERS, &BLOBS];
static SCHEMA: Schema<'static> = Schema { tables: &TABLES };

fn ty(storage: Storage, rust: RustTy<'static>, nullable: bool) -> Ty<'static> {
    Ty { storage, rust, nullable }
}

fn scalar(name: &'static str, ty: Ty<'static>) -> Param<'static> {
    Param { name, ty, list: false }
}

fn col(name: &'static str, ty: Ty<'static>) -> ResultCol<'static> {
    ResultCol { name, ty }
}

fn describe(text: &'static str) -> Description<'static, Global> {
    let statement = parse(text, Global).unwrap_or_else(|error| panic!("{text}: {error}"));
    infer(&SCHEMA, &statement, Global).unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn refuse(text: &'static str) -> TypeError<'static> {
    let statement = parse(text, Global).unwrap_or_else(|error| panic!("{text}: {error}"));
    match infer(&SCHEMA, &statement, Global) {
        Ok(description) => panic!("{text} typed, as {description:?}"),
        Err(error) => error,
    }
}

fn rows<'d>(description: &'d Description<'static, Global>) -> &'d [ResultCol<'static>] {
    match &description.returns {
        Returns::Rows(columns) => columns,
        Returns::Count => panic!("returns a count, not rows"),
    }
}

/// The error's message names each of `words`.
fn says(error: &TypeError<'_>, words: &[&str]) {
    let message = format!("{error}");
    for word in words {
        assert!(message.contains(word), "{message:?} does not name {word:?}");
    }
}

// Scope.

#[test]
fn an_unqualified_column_names_one_table_in_scope() {
    let d = describe("select person.id from person join orders on orders.person = person.id");
    assert_eq!(rows(&d), [col("id", ty(Storage::Integer, PERSON_ID, false))]);
    let error = refuse("select id from person join orders on orders.person = person.id");
    assert!(matches!(
        error.kind,
        TypeErrorKind::AmbiguousColumn { column: "id", first: "person", second: "orders" }
    ));
    says(&error, &["`id`", "`person`", "`orders`"]);
}

#[test]
fn a_table_is_in_scope_under_its_alias() {
    let d = describe("select p.name from person as p");
    assert_eq!(rows(&d), [col("name", ty(Storage::Text, STRING, false))]);
    let error = refuse("select person.name from person as p");
    assert!(matches!(error.kind, TypeErrorKind::NotInScope { table: "person" }));
    says(&error, &["`person`"]);
    let error = refuse("select 1 as x from person join person on true");
    assert!(matches!(error.kind, TypeErrorKind::TableTwice { name: "person" }));
}

#[test]
fn unknown_tables_and_columns_are_refused_by_name() {
    let error = refuse("select 1 as x from nope");
    assert!(matches!(error.kind, TypeErrorKind::NoSuchTable { table: "nope" }));
    says(&error, &["`nope`"]);
    let error = refuse("select nope from person");
    assert!(matches!(error.kind, TypeErrorKind::UnknownColumn { column: "nope" }));
    says(&error, &["`nope`"]);
    let error = refuse("select person.nope from person");
    assert!(matches!(error.kind, TypeErrorKind::NoSuchColumn { table: "person", column: "nope" }));
}

#[test]
fn a_left_joins_right_side_is_nullable_and_its_left_side_is_not() {
    let d = describe(
        "select orders.total as t, orders.note as o, person.name as n from person left join orders on orders.person = person.id",
    );
    assert_eq!(
        rows(&d),
        [
            col("t", ty(Storage::Real, F64, true)),
            col("o", ty(Storage::Text, STRING, true)),
            col("n", ty(Storage::Text, STRING, false)),
        ]
    );
    let d =
        describe("select orders.total as t from person join orders on orders.person = person.id");
    assert_eq!(rows(&d), [col("t", ty(Storage::Real, F64, false))]);
}

// Literals and operators.

#[test]
fn literals_are_their_storage_and_non_null() {
    let d = describe(
        "select true as b, 1 as i, 1.5 as r, 'a' as t, x'00' as x, -9223372036854775808 as m",
    );
    assert_eq!(
        rows(&d),
        [
            col("b", ty(Storage::Boolean, BOOL, false)),
            col("i", ty(Storage::Integer, I64, false)),
            col("r", ty(Storage::Real, F64, false)),
            col("t", ty(Storage::Text, STRING, false)),
            col("x", ty(Storage::Blob, BYTES, false)),
            col("m", ty(Storage::Integer, I64, false)),
        ]
    );
    let error = refuse("select 9223372036854775808 as x");
    assert!(matches!(
        error.kind,
        TypeErrorKind::IntegerTooLarge { magnitude: 9223372036854775808 }
    ));
    says(&error, &["9223372036854775808"]);
}

#[test]
fn neg_takes_a_number_and_not_a_boolean() {
    let d = describe(
        "select -age as n, -total as t, not active as a from person join orders on orders.person = person.id",
    );
    assert_eq!(
        rows(&d),
        [
            col("n", ty(Storage::Integer, I64, false)),
            col("t", ty(Storage::Real, F64, false)),
            col("a", ty(Storage::Boolean, BOOL, false)),
        ]
    );
    let error = refuse("select -name as n from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage {
            what: What::Column("name"),
            expected: Expected::Number,
            found: Storage::Text,
            ..
        }
    ));
    says(&error, &["`name`", "`-`"]);
    let error = refuse("select not age as n from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage {
            expected: Expected::One(Storage::Boolean),
            found: Storage::Integer,
            ..
        }
    ));
}

#[test]
fn and_or_are_nullable_if_an_operand_is() {
    let d = describe(
        "select active and city = 'x' as b, active or active as c, active and $f as d from person",
    );
    assert_eq!(
        rows(&d),
        [
            col("b", ty(Storage::Boolean, BOOL, true)),
            col("c", ty(Storage::Boolean, BOOL, false)),
            col("d", ty(Storage::Boolean, BOOL, false)),
        ]
    );
    assert_eq!(d.params[..], [scalar("f", ty(Storage::Boolean, BOOL, false))]);
}

#[test]
fn arithmetic_is_one_storage_with_the_plain_type() {
    let d = describe(
        "select age + 1 as a, age / 2 as d, age % 3 as m, total * 2.0 as t, age - $n as s, person.id + 1 as next from person join orders on orders.person = person.id",
    );
    assert_eq!(
        rows(&d),
        [
            col("a", ty(Storage::Integer, I64, false)),
            col("d", ty(Storage::Integer, I64, true)),
            col("m", ty(Storage::Integer, I64, true)),
            col("t", ty(Storage::Real, F64, false)),
            col("s", ty(Storage::Integer, I64, false)),
            col("next", ty(Storage::Integer, I64, false)),
        ]
    );
    assert_eq!(d.params[..], [scalar("n", ty(Storage::Integer, I64, false))]);
    let error = refuse("select age + 1.5 as x from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::MixedArithmetic { op: "+", left: Storage::Integer, right: Storage::Real }
    ));
    says(&error, &["`+`", "cast"]);
    let error = refuse("select total % 2.0 as x from orders");
    assert!(matches!(error.kind, TypeErrorKind::RealRemainder));
    let error = refuse("select $a + $b as x");
    assert!(matches!(error.kind, TypeErrorKind::ParamsBothSides { left: "a", right: "b", .. }));
    says(&error, &["$a", "$b", "cast"]);
}

#[test]
fn concat_takes_two_texts() {
    let d = describe("select name || '!' as n, city || name as c, name || $s as p from person");
    assert_eq!(
        rows(&d),
        [
            col("n", ty(Storage::Text, STRING, false)),
            col("c", ty(Storage::Text, STRING, true)),
            col("p", ty(Storage::Text, STRING, false)),
        ]
    );
    assert_eq!(d.params[..], [scalar("s", ty(Storage::Text, STRING, false))]);
    let error = refuse("select name || age as x from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage {
            what: What::Column("age"),
            expected: Expected::One(Storage::Text),
            ..
        }
    ));
}

#[test]
fn a_long_chain_types_and_renders_as_one_node() {
    // 1,000 `and`-ed comparisons, each with one parameter.
    let ands: &'static str =
        format!("select name from person where age = $a{}", " and age = $a".repeat(999)).leak();
    let d = describe(ands);
    assert_eq!(d.params[..], [scalar("a", ty(Storage::Integer, I64, false))]);
    assert_eq!(rows(&d), [col("name", ty(Storage::Text, STRING, false))]);
    let (sqlite, slots) = text_of(ands, Backend::Sqlite, &[]);
    let terms: Vec<&str> = vec![r#"("age" = ?1)"#; 1000];
    assert_eq!(
        sqlite,
        format!(r#"select "name" as "name" from "person" where {}"#, terms.join(" and "))
    );
    assert_eq!(slots, [Slot::Scalar { param: 0 }]);
    let (postgres, _) = text_of(ands, Backend::Postgres, &[]);
    let terms: Vec<&str> = vec![r#"("age" = $1)"#; 1000];
    assert_eq!(
        postgres,
        format!(r#"select "name" as "name" from "person" where {}"#, terms.join(" and "))
    );
    // A 1,000-term sum, a parameter among its terms.
    let sum: &'static str =
        format!("select age + $n{} as s from person", " + age".repeat(998)).leak();
    let d = describe(sum);
    assert_eq!(d.params[..], [scalar("n", ty(Storage::Integer, I64, false))]);
    assert_eq!(rows(&d), [col("s", ty(Storage::Integer, I64, false))]);
    let (sqlite, _) = text_of(sum, Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        format!(r#"select "age" + ?1{} as "s" from "person""#, r#" + "age""#.repeat(998))
    );
    let (postgres, _) = text_of(sum, Backend::Postgres, &[]);
    assert_eq!(
        postgres,
        format!(r#"select "age" + $1{} as "s" from "person""#, r#" + "age""#.repeat(998))
    );
    // A link of the wrong storage is refused, naming it, however far along the chain.
    let bad: &'static str =
        format!("select age{} + name as s from person", " + age".repeat(500)).leak();
    let error = refuse(bad);
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage { what: What::Column("name"), expected: Expected::Number, .. }
    ));
}

#[test]
fn a_parameter_compared_with_a_newtype_column_is_that_newtype() {
    let d = describe("select name from person where id = $id");
    assert_eq!(d.params[..], [scalar("id", ty(Storage::Integer, PERSON_ID, false))]);
    assert_eq!(rows(&d), [col("name", ty(Storage::Text, STRING, false))]);
    let d = describe("select id = 5 as b, city = 'x' as c from person");
    assert_eq!(
        rows(&d),
        [col("b", ty(Storage::Boolean, BOOL, false)), col("c", ty(Storage::Boolean, BOOL, true))]
    );
}

#[test]
fn person_id_and_order_id_do_not_compare() {
    let error = refuse("select person.name from person join orders on person.id = orders.id");
    assert!(matches!(
        error.kind,
        TypeErrorKind::CompareTypes { left: "PersonId", right: "OrderId", .. }
    ));
    says(&error, &["`PersonId`", "`OrderId`"]);
    let error = refuse("select name = 1 as b from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::CompareStorage { left: Storage::Text, right: Storage::Integer, .. }
    ));
    let error = refuse("select 1 as x from person where $a = $b");
    assert!(matches!(error.kind, TypeErrorKind::ParamsBothSides { left: "a", right: "b", .. }));
    says(&error, &["$a", "$b", "cast"]);
}

#[test]
fn is_is_non_null_and_its_parameter_nullable() {
    let d = describe("select city is $c as m, name is not $n as k from person");
    assert_eq!(
        rows(&d),
        [col("m", ty(Storage::Boolean, BOOL, false)), col("k", ty(Storage::Boolean, BOOL, false))]
    );
    assert_eq!(
        d.params[..],
        [
            scalar("c", ty(Storage::Text, STRING, true)),
            scalar("n", ty(Storage::Text, STRING, true))
        ]
    );
}

#[test]
fn is_null_takes_a_typed_operand() {
    let d = describe("select city is null as n, name is not null as m from person");
    assert_eq!(
        rows(&d),
        [col("n", ty(Storage::Boolean, BOOL, false)), col("m", ty(Storage::Boolean, BOOL, false))]
    );
    let error = refuse("select 1 as x from person where $p is null");
    assert!(matches!(error.kind, TypeErrorKind::NeedsCast { what: What::Param("p"), .. }));
    says(&error, &["$p", "cast"]);
}

#[test]
fn in_and_between_compare_each_entry() {
    let d = describe("select name from person where id in ($a, $b, 3)");
    assert_eq!(
        d.params[..],
        [
            scalar("a", ty(Storage::Integer, PERSON_ID, false)),
            scalar("b", ty(Storage::Integer, PERSON_ID, false)),
        ]
    );
    let d = describe("select name from person where id not in $ids");
    assert_eq!(
        d.params[..],
        [Param { name: "ids", ty: ty(Storage::Integer, PERSON_ID, false), list: true }]
    );
    let d =
        describe("select city in ('a', 'b') as c from person where age between $lo and $hi + 1");
    assert_eq!(rows(&d), [col("c", ty(Storage::Boolean, BOOL, true))]);
    assert_eq!(
        d.params[..],
        [
            scalar("lo", ty(Storage::Integer, I64, false)),
            scalar("hi", ty(Storage::Integer, I64, false))
        ]
    );
    let error = refuse("select name from person where name in (1, 2)");
    assert!(matches!(error.kind, TypeErrorKind::CompareStorage { .. }));
}

#[test]
fn like_takes_two_texts() {
    let d = describe("select name like $p as m, city like 'a%' as c from person");
    assert_eq!(
        rows(&d),
        [col("m", ty(Storage::Boolean, BOOL, false)), col("c", ty(Storage::Boolean, BOOL, true))]
    );
    assert_eq!(d.params[..], [scalar("p", ty(Storage::Text, STRING, false))]);
    let error = refuse("select age like 'a' as x from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage {
            what: What::Column("age"),
            expected: Expected::One(Storage::Text),
            ..
        }
    ));
}

#[test]
fn case_branches_have_one_type() {
    let d = describe(
        "select case when active then name else city end as c, case age when 1 then 'one' end as w, case when active then $x else name end as p from person",
    );
    assert_eq!(
        rows(&d),
        [
            col("c", ty(Storage::Text, STRING, true)),
            col("w", ty(Storage::Text, STRING, true)),
            col("p", ty(Storage::Text, STRING, false)),
        ]
    );
    assert_eq!(d.params[..], [scalar("x", ty(Storage::Text, STRING, false))]);
    let error = refuse("select case when active then name else age end as c from person");
    assert!(matches!(error.kind, TypeErrorKind::BranchTypes { .. }));
    says(&error, &["`String`", "`i64`"]);
    let error = refuse("select case when active then $a else $b end as c from person");
    assert!(matches!(error.kind, TypeErrorKind::ParamsBothSides { left: "a", right: "b", .. }));
}

#[test]
fn cast_gives_its_type_and_a_parameter_takes_it() {
    let d = describe(
        "select cast($n as integer) as n, cast(city as blob) as b, cast(age as text) as t, cast(active as boolean) as a, cast($f as boolean) as f from person",
    );
    assert_eq!(
        rows(&d),
        [
            col("n", ty(Storage::Integer, I64, false)),
            col("b", ty(Storage::Blob, BYTES, true)),
            col("t", ty(Storage::Text, STRING, false)),
            col("a", ty(Storage::Boolean, BOOL, false)),
            col("f", ty(Storage::Boolean, BOOL, false)),
        ]
    );
    assert_eq!(
        d.params[..],
        [
            scalar("n", ty(Storage::Integer, I64, false)),
            scalar("f", ty(Storage::Boolean, BOOL, false))
        ]
    );
}

#[test]
fn only_a_boolean_or_a_parameter_casts_to_boolean() {
    let error = refuse("select cast(age as boolean) as b from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::CastToBoolean { what: What::Column("age"), found: Storage::Integer }
    ));
    says(&error, &["`age`", "boolean", "<> 0"]);
}

#[test]
fn aggregates_have_their_types() {
    let d = describe(
        "select count(*) as a, count(city) as b, count(distinct city) as c, sum(age) as d, sum(total) as e, min(person.id) as f, max(name) as g, avg(age) as h from person join orders on orders.person = person.id",
    );
    assert_eq!(
        rows(&d),
        [
            col("a", ty(Storage::Integer, I64, false)),
            col("b", ty(Storage::Integer, I64, false)),
            col("c", ty(Storage::Integer, I64, false)),
            col("d", ty(Storage::Integer, I64, true)),
            col("e", ty(Storage::Real, F64, true)),
            col("f", ty(Storage::Integer, PERSON_ID, true)),
            col("g", ty(Storage::Text, STRING, true)),
            col("h", ty(Storage::Real, F64, true)),
        ]
    );
    let error = refuse("select max(active) as m from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage { expected: Expected::NotBoolean, .. }
    ));
    let error = refuse("select sum(name) as s from person");
    assert!(matches!(error.kind, TypeErrorKind::WrongStorage { expected: Expected::Number, .. }));
}

#[test]
fn scalar_functions_have_their_types() {
    let d = describe(
        "select coalesce(city, name) as a, coalesce(city, city) as b, coalesce(city, $d) as c, lower(name) as l, upper(city) as u, length(name) as n, abs(age) as x, abs(-1.5) as y from person",
    );
    assert_eq!(
        rows(&d),
        [
            col("a", ty(Storage::Text, STRING, false)),
            col("b", ty(Storage::Text, STRING, true)),
            col("c", ty(Storage::Text, STRING, false)),
            col("l", ty(Storage::Text, STRING, false)),
            col("u", ty(Storage::Text, STRING, true)),
            col("n", ty(Storage::Integer, I64, false)),
            col("x", ty(Storage::Integer, I64, false)),
            col("y", ty(Storage::Real, F64, false)),
        ]
    );
    assert_eq!(d.params[..], [scalar("d", ty(Storage::Text, STRING, false))]);
}

#[test]
fn calls_outside_the_function_table_are_refused() {
    let error = refuse("select foo(1) as x");
    assert!(matches!(error.kind, TypeErrorKind::UnknownFunction { name: "foo" }));
    says(&error, &["`foo`", "count", "coalesce", "abs"]);
    let error = refuse("select lower(distinct name) as x from person");
    assert!(matches!(error.kind, TypeErrorKind::DistinctNotAggregate { function: "lower" }));
    let error = refuse("select sum(*) as x from person");
    assert!(matches!(error.kind, TypeErrorKind::StarNotCount { function: "sum" }));
    let error = refuse("select coalesce(name) as x from person");
    assert!(matches!(error.kind, TypeErrorKind::Arity { function: "coalesce", found: 1, .. }));
    says(&error, &["`coalesce`", "at least 2"]);
}

#[test]
fn aggregates_are_refused_in_where_on_group_by_and_each_other() {
    let error = refuse("select name from person where count(*) > 1");
    assert!(matches!(
        error.kind,
        TypeErrorKind::AggregateNotAllowed { function: "count", place: Place::Clause("where") }
    ));
    says(&error, &["`count`", "`where`"]);
    let error = refuse("select sum(count(*)) as x from person");
    assert!(matches!(
        error.kind,
        TypeErrorKind::AggregateNotAllowed { function: "count", place: Place::InsideAggregate }
    ));
    let error = refuse("select count(*) as n from person group by count(*)");
    assert!(matches!(
        error.kind,
        TypeErrorKind::AggregateNotAllowed { place: Place::Clause("group by"), .. }
    ));
    let error = refuse("select person.name from person join orders on count(*) > 0");
    assert!(matches!(
        error.kind,
        TypeErrorKind::AggregateNotAllowed { place: Place::Clause("on"), .. }
    ));
    let error = refuse("update person set age = count(*)");
    assert!(matches!(
        error.kind,
        TypeErrorKind::AggregateNotAllowed { place: Place::Clause("set"), .. }
    ));
}

#[test]
fn a_grouped_select_reads_only_grouped_columns_outside_aggregates() {
    let d = describe("select city, count(*) as n from person group by city having count(*) > 1");
    assert_eq!(
        rows(&d),
        [col("city", ty(Storage::Text, STRING, true)), col("n", ty(Storage::Integer, I64, false))]
    );
    describe("select age + 1 as a, count(*) as n from person group by age + 1");
    describe("select person.city as c from person group by city");
    let error = refuse("select name, count(*) as n from person group by city");
    assert!(matches!(error.kind, TypeErrorKind::NotGrouped { column: "name" }));
    says(&error, &["`name`", "group by"]);
    let error = refuse("select count(*) as n, age from person");
    assert!(matches!(error.kind, TypeErrorKind::NotGrouped { column: "age" }));
    let error = refuse("select * from person group by city");
    assert!(matches!(error.kind, TypeErrorKind::NotGrouped { column: "id" }));
    let error = refuse("select city from person group by city order by age");
    assert!(matches!(error.kind, TypeErrorKind::NotGrouped { column: "age" }));
    let error = refuse("select name from person having true");
    assert!(matches!(error.kind, TypeErrorKind::HavingWithoutGroup));
}

#[test]
fn where_is_boolean_and_limit_an_integer_never_null() {
    let d = describe("select name from person where $flag limit $n offset $o");
    assert_eq!(
        d.params[..],
        [
            scalar("flag", ty(Storage::Boolean, BOOL, false)),
            scalar("n", ty(Storage::Integer, I64, false)),
            scalar("o", ty(Storage::Integer, I64, false)),
        ]
    );
    let error = refuse("select name from person where age");
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage {
            expected: Expected::One(Storage::Boolean),
            found: Storage::Integer,
            place: Place::Clause("where"),
            ..
        }
    ));
    let error = refuse("select name from person limit 'a'");
    assert!(matches!(
        error.kind,
        TypeErrorKind::WrongStorage { place: Place::Clause("limit"), .. }
    ));
    let error = refuse("select name from person limit age");
    assert!(matches!(error.kind, TypeErrorKind::ColumnNotAllowed { column: "age", .. }));
    let error = refuse("select name from person limit null");
    assert!(matches!(error.kind, TypeErrorKind::NullableNotAllowed { .. }));
}

#[test]
fn result_columns_are_named_and_stars_expand() {
    let d = describe("select * from person");
    assert_eq!(
        rows(&d),
        [
            col("id", ty(Storage::Integer, PERSON_ID, false)),
            col("name", ty(Storage::Text, STRING, false)),
            col("age", ty(Storage::Integer, I64, false)),
            col("city", ty(Storage::Text, STRING, true)),
            col("active", ty(Storage::Boolean, BOOL, false)),
        ]
    );
    let d =
        describe("select o.*, p.name from person as p left join orders as o on o.person = p.id");
    assert_eq!(
        rows(&d),
        [
            col("id", ty(Storage::Integer, ORDER_ID, true)),
            col("person", ty(Storage::Integer, PERSON_ID, true)),
            col("total", ty(Storage::Real, F64, true)),
            col("note", ty(Storage::Text, STRING, true)),
            col("name", ty(Storage::Text, STRING, false)),
        ]
    );
    let error = refuse("select age + 1 from person");
    assert!(matches!(error.kind, TypeErrorKind::Unnamed { .. }));
    says(&error, &["`+`", "as"]);
    let error = refuse("select name, city as NAME from person");
    assert!(matches!(error.kind, TypeErrorKind::DuplicateResultName { name: "NAME" }));
    let error = refuse("select * from person join orders on orders.person = person.id");
    assert!(matches!(error.kind, TypeErrorKind::DuplicateResultName { name: "id" }));
    let error = refuse("select *");
    assert!(matches!(error.kind, TypeErrorKind::StarWithoutFrom));
}

#[test]
fn order_by_names_an_alias_or_an_expression() {
    describe("select age + 1 as a from person order by a desc");
    describe("select distinct name from person order by name");
    let error = refuse("select distinct name from person order by age");
    assert!(matches!(error.kind, TypeErrorKind::NotInDistinct { what: What::Column("age") }));
    let error = refuse("select name from person order by 1");
    assert!(matches!(error.kind, TypeErrorKind::Positional { clause: "order by", position: 1 }));
}

// Statements that change rows.

#[test]
fn insert_types_each_value_to_its_column() {
    let d =
        describe("insert into person (name, age, city, active) values ($name, $age, $city, true)");
    assert_eq!(
        d.params[..],
        [
            scalar("name", ty(Storage::Text, STRING, false)),
            scalar("age", ty(Storage::Integer, I64, false)),
            scalar("city", ty(Storage::Text, STRING, true)),
        ]
    );
    assert!(matches!(d.returns, Returns::Count));
    let d = describe("insert into orders (person, total) values ($p, $t), ($p, 2.5)");
    assert_eq!(
        d.params[..],
        [
            scalar("p", ty(Storage::Integer, PERSON_ID, false)),
            scalar("t", ty(Storage::Real, F64, false))
        ]
    );
    let d = describe(
        "insert into person (id, name, age, active) values ($id, 'a', 1, true) returning id, name",
    );
    assert_eq!(d.params[..], [scalar("id", ty(Storage::Integer, PERSON_ID, false))]);
    assert_eq!(
        rows(&d),
        [
            col("id", ty(Storage::Integer, PERSON_ID, false)),
            col("name", ty(Storage::Text, STRING, false))
        ]
    );
}

#[test]
fn insert_must_list_every_non_nullable_column_but_the_integer_key() {
    let error = refuse("insert into person (name) values ('a')");
    assert!(matches!(error.kind, TypeErrorKind::MissingColumn { table: "person", column: "age" }));
    says(&error, &["`person`", "`age`"]);
    let error = refuse("insert into person (name, name, age, active) values ('a', 'b', 1, true)");
    assert!(matches!(error.kind, TypeErrorKind::ColumnTwice { column: "name" }));
    let error = refuse("insert into person (name, age, active) values ('a', 1)");
    assert!(matches!(error.kind, TypeErrorKind::RowWidth { row: 1, columns: 3, values: 2 }));
    let error = refuse("insert into person (name, age, active) values (null, 1, true)");
    assert!(matches!(error.kind, TypeErrorKind::NullIntoNonNull { column: "name" }));
    let error = refuse("insert into person (name, age, active) values (name, 1, true)");
    assert!(matches!(error.kind, TypeErrorKind::ColumnNotAllowed { column: "name", .. }));
    let error = refuse("insert into orders (person, total) values (1 + 1, 2.0)");
    assert!(matches!(error.kind, TypeErrorKind::AssignType { column: "person", .. }));
    says(&error, &["`person`", "`PersonId`", "`i64`"]);
    let error = refuse("insert into orders (person, total) values ('a', 2.0)");
    assert!(matches!(error.kind, TypeErrorKind::AssignType { column: "person", .. }));
}

#[test]
fn a_literal_goes_into_a_column_of_its_storage_whatever_its_rust_type() {
    let d = describe("insert into orders (id, person, total) values (1, -2, 2.0)");
    assert!(d.params.is_empty());
    assert!(matches!(d.returns, Returns::Count));
    let d = describe("update orders set person = 3, total = -1.5 where id = $id");
    assert_eq!(d.params[..], [scalar("id", ty(Storage::Integer, ORDER_ID, false))]);
    let error = refuse("update orders set person = abs(3)");
    assert!(matches!(error.kind, TypeErrorKind::AssignType { column: "person", .. }));
}

#[test]
fn update_types_each_set_and_returning_gives_rows() {
    let d = describe("update person set age = $age, city = $city where id = $id returning id");
    assert_eq!(
        d.params[..],
        [
            scalar("age", ty(Storage::Integer, I64, false)),
            scalar("city", ty(Storage::Text, STRING, true)),
            scalar("id", ty(Storage::Integer, PERSON_ID, false)),
        ]
    );
    assert_eq!(rows(&d), [col("id", ty(Storage::Integer, PERSON_ID, false))]);
    let error = refuse("update person set age = age, age = 1");
    assert!(matches!(error.kind, TypeErrorKind::ColumnTwice { column: "age" }));
    let error = refuse("update person set name = city");
    assert!(matches!(error.kind, TypeErrorKind::NullableIntoNonNull { column: "name", .. }));
}

#[test]
fn delete_types_its_filter_and_returns_a_count() {
    let d = describe("delete from orders where person = $p");
    assert_eq!(d.params[..], [scalar("p", ty(Storage::Integer, PERSON_ID, false))]);
    assert!(matches!(d.returns, Returns::Count));
}

// Parameters.

#[test]
fn parameters_are_ordered_by_first_appearance() {
    let d = describe("select name from person where $b = age and id = $a and age < $b");
    assert_eq!(
        d.params[..],
        [
            scalar("b", ty(Storage::Integer, I64, false)),
            scalar("a", ty(Storage::Integer, PERSON_ID, false))
        ]
    );
}

#[test]
fn one_name_at_two_types_is_refused() {
    let error = refuse("select name from person where age = $x and name = $x");
    assert!(matches!(error.kind, TypeErrorKind::ParamConflict { name: "x", .. }));
    says(&error, &["$x", "`i64`", "`String`"]);
    let error = refuse("select name from person where city = $c or city is $c");
    assert!(matches!(error.kind, TypeErrorKind::ParamConflict { name: "c", .. }));
    says(&error, &["`String`", "`Option<String>`"]);
}

#[test]
fn a_parameter_no_position_types_is_refused() {
    let error = refuse("select $x as x");
    assert!(matches!(
        error.kind,
        TypeErrorKind::NeedsCast { what: What::Param("x"), place: Place::Result }
    ));
    says(&error, &["$x", "cast"]);
}

// Properties.

/// The oracle: `params` is the one parameter `$p` with `column`'s storage and Rust type,
/// non-null for `=` and nullable for `is`.
fn compared_param_holds(column: &Column<'static>, is: bool, params: &[Param<'static>]) -> bool {
    params == [scalar("p", ty(column.storage, column.rust, is))]
}

/// The invariant: a parameter compared with a column takes the column's storage and Rust type
/// (its newtype, not the plain type), non-null for `=` and nullable for `is`.
#[test]
fn a_compared_parameter_takes_its_columns_type() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let table = *tape.one_of(&TABLES, "table");
        let column = &table.columns[tape.index(table.columns.len(), "column")];
        let is = tape.bool("is");
        let text: &'static str = format!(
            "select 1 as one from {} where {} {} $p",
            table.name,
            column.name,
            if is { "is" } else { "=" }
        )
        .leak();
        let d = describe(text);
        assert!(compared_param_holds(column, is, &d.params), "{text}: {:?}", d.params);
    });
}

#[test]
fn the_oracle_catches_a_parameter_given_the_plain_type() {
    let planted = [scalar("p", ty(Storage::Integer, I64, false))];
    assert!(!compared_param_holds(&PERSON_COLUMNS[0], false, &planted));
}

/// The oracle: the `left join`'s right column `v` is nullable with its column's type, and the
/// left side's `n` keeps its non-null type.
fn left_join_holds(columns: &[ResultCol<'static>], column: &Column<'static>) -> bool {
    columns
        == [
            col("v", ty(column.storage, column.rust, true)),
            col("n", ty(Storage::Text, STRING, false)),
        ]
}

fn left_join_text(column: &Column<'static>) -> &'static str {
    format!(
        "select orders.{} as v, person.name as n from person left join orders on orders.person = person.id",
        column.name
    )
    .leak()
}

/// The invariant: every column of a `left join`'s right side is nullable, and the left side's
/// columns keep their own nullability.
#[test]
fn a_left_joins_right_columns_are_nullable() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let column = &ORDERS_COLUMNS[tape.index(ORDERS_COLUMNS.len(), "column")];
        let d = describe(left_join_text(column));
        assert!(left_join_holds(rows(&d), column), "{:?}", rows(&d));
    });
}

#[cfg(feature = "plant")]
#[test]
fn the_oracle_catches_a_left_join_that_keeps_non_null() {
    use crate::infer::{Plant, infer_planted};
    let column = &ORDERS_COLUMNS[2];
    let text = left_join_text(column);
    let statement = parse(text, Global).unwrap();
    let planted = infer_planted(&SCHEMA, &statement, Plant::LeftJoinKeepsNonNull, Global).unwrap();
    assert!(!left_join_holds(rows(&planted), column));
    // The plant changes exactly the right side's nullability.
    assert_eq!(
        rows(&planted),
        [col("v", ty(Storage::Real, F64, false)), col("n", ty(Storage::Text, STRING, false))]
    );
    assert_eq!(planted.params[..], describe(text).params[..]);
}

// Rendering.

fn rendered(
    text: &'static str,
    backend: Backend,
    lens: &[u32],
) -> Result<(String, Vec<Slot>), RenderError> {
    let checked =
        crate::check(&SCHEMA, text, Global).unwrap_or_else(|error| panic!("{text}: {error}"));
    let out = render(&checked, backend, lens, Global)?;
    Ok((String::from_utf8(out.text.as_slice().to_vec()).unwrap(), out.slots.as_slice().to_vec()))
}

fn text_of(text: &'static str, backend: Backend, lens: &[u32]) -> (String, Vec<Slot>) {
    rendered(text, backend, lens).unwrap_or_else(|error| panic!("{text}: {error}"))
}

#[test]
fn a_repeated_parameter_has_one_placeholder() {
    let text = "select name from person where age > $min and age < $min + 10";
    let (sqlite, slots) = text_of(text, Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        r#"select "name" as "name" from "person" where ("age" > ?1) and ("age" < (?1 + 10))"#
    );
    assert_eq!(slots, [Slot::Scalar { param: 0 }]);
    let (postgres, slots) = text_of(text, Backend::Postgres, &[]);
    assert_eq!(
        postgres,
        r#"select "name" as "name" from "person" where ("age" > $1) and ("age" < ($1 + 10::bigint))"#
    );
    assert_eq!(slots, [Slot::Scalar { param: 0 }]);
}

#[test]
fn a_list_parameter_is_a_placeholder_per_entry_or_an_array() {
    let text = "select name from person where id in $ids and age > $a";
    let (sqlite, slots) = text_of(text, Backend::Sqlite, &[3]);
    assert_eq!(
        sqlite,
        r#"select "name" as "name" from "person" where ("id" in (?1, ?2, ?3)) and ("age" > ?4)"#
    );
    assert_eq!(
        slots,
        [
            Slot::ListItem { param: 0, index: 0 },
            Slot::ListItem { param: 0, index: 1 },
            Slot::ListItem { param: 0, index: 2 },
            Slot::Scalar { param: 1 },
        ]
    );
    let (sqlite, slots) = text_of(text, Backend::Sqlite, &[0]);
    assert_eq!(
        sqlite,
        r#"select "name" as "name" from "person" where ("id" in ()) and ("age" > ?1)"#
    );
    assert_eq!(slots, [Slot::Scalar { param: 1 }]);
    let (postgres, slots) = text_of(text, Backend::Postgres, &[3]);
    assert_eq!(
        postgres,
        r#"select "name" as "name" from "person" where ("id" = any($1)) and ("age" > $2)"#
    );
    assert_eq!(slots, [Slot::Array { param: 0 }, Slot::Scalar { param: 1 }]);
    let (postgres, _) =
        text_of("select name from person where id not in $ids", Backend::Postgres, &[2]);
    assert_eq!(postgres, r#"select "name" as "name" from "person" where "id" <> all($1)"#);
}

#[test]
fn a_list_parameter_is_used_once_on_sqlite_and_its_lengths_match() {
    let text = "select name from person where id in $ids or id in $ids";
    let error = rendered(text, Backend::Sqlite, &[2]).unwrap_err();
    assert!(matches!(&error, RenderError::ListUsedTwice { param, .. } if param.as_str() == "ids"));
    assert!(format!("{error}").contains("$ids"));
    let (postgres, slots) = text_of(text, Backend::Postgres, &[2]);
    assert_eq!(
        postgres,
        r#"select "name" as "name" from "person" where ("id" = any($1)) or ("id" = any($1))"#
    );
    assert_eq!(slots, [Slot::Array { param: 0 }]);
    let error = rendered(text, Backend::Sqlite, &[]).unwrap_err();
    assert!(matches!(error, RenderError::ListLens { expected: 1, found: 0 }));
}

#[test]
fn is_is_null_safe_equality_on_both_engines() {
    let text = "select name from person where city is $c and city is not $c";
    let (sqlite, _) = text_of(text, Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        r#"select "name" as "name" from "person" where ("city" is ?1) and ("city" is not ?1)"#
    );
    let (postgres, _) = text_of(text, Backend::Postgres, &[]);
    assert_eq!(
        postgres,
        r#"select "name" as "name" from "person" where ("city" is not distinct from $1) and ("city" is distinct from $1)"#
    );
}

#[test]
fn literals_are_written_as_each_engine_reads_them() {
    let text =
        "select 1 as i, 1.5 as r, 'a''b' as t, x'0aff' as b, true as y, -9223372036854775808 as m";
    let (sqlite, _) = text_of(text, Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        r#"select 1 as "i", 1.5 as "r", 'a''b' as "t", x'0aff' as "b", true as "y", -9223372036854775808 as "m""#
    );
    let (postgres, _) = text_of(text, Backend::Postgres, &[]);
    assert_eq!(
        postgres,
        r#"select 1::bigint as "i", 1.5::double precision as "r", 'a''b' as "t", '\x0aff'::bytea as "b", true as "y", (-9223372036854775808)::bigint as "m""#
    );
}

#[test]
fn postgres_aggregates_are_cast_to_sqlites_types() {
    let text = "select sum(age) as s, sum(total) as t, avg(age) as a, count(*) as n from person join orders on orders.person = person.id";
    let (sqlite, _) = text_of(text, Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        r#"select sum("age") as "s", sum("total") as "t", avg("age") as "a", count(*) as "n" from "person" join "orders" on "orders"."person" = "person"."id""#
    );
    let (postgres, _) = text_of(text, Backend::Postgres, &[]);
    assert_eq!(
        postgres,
        r#"select cast(sum("age") as bigint) as "s", sum("total") as "t", cast(avg("age") as double precision) as "a", cast(count(*) as bigint) as "n" from "person" join "orders" on "orders"."person" = "person"."id""#
    );
    let (postgres, _) = text_of("select length(name) as l from person", Backend::Postgres, &[]);
    assert_eq!(postgres, r#"select cast(length("name") as bigint) as "l" from "person""#);
}

#[test]
fn cast_names_each_engines_type() {
    let text = "select cast(age as real) as r, cast(active as boolean) as b, cast($p as boolean) as p, cast(age as text) as t from person";
    let (sqlite, slots) = text_of(text, Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        r#"select cast("age" as real) as "r", cast("active" as integer) as "b", cast(?1 as integer) as "p", cast("age" as text) as "t" from "person""#
    );
    assert_eq!(slots, [Slot::Scalar { param: 0 }]);
    let (postgres, _) = text_of(text, Backend::Postgres, &[]);
    assert_eq!(
        postgres,
        r#"select cast("age" as double precision) as "r", cast("active" as boolean) as "b", cast($1 as boolean) as "p", cast("age" as text) as "t" from "person""#
    );
}

#[test]
fn every_result_column_is_written_with_its_name() {
    let (sqlite, _) = text_of("select * from person", Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        r#"select "person"."id" as "id", "person"."name" as "name", "person"."age" as "age", "person"."city" as "city", "person"."active" as "active" from "person""#
    );
    let (sqlite, _) = text_of(
        "select o.*, p.name from person as p join orders as o on o.person = p.id",
        Backend::Sqlite,
        &[],
    );
    assert_eq!(
        sqlite,
        r#"select "o"."id" as "id", "o"."person" as "person", "o"."total" as "total", "o"."note" as "note", "p"."name" as "name" from "person" as "p" join "orders" as "o" on "o"."person" = "p"."id""#
    );
    let (sqlite, slots) = text_of(
        "insert into person (name, age, active) values ($n, 1, true) returning id",
        Backend::Sqlite,
        &[],
    );
    assert_eq!(
        sqlite,
        r#"insert into "person" ("name", "age", "active") values (?1, 1, true) returning "id" as "id""#
    );
    assert_eq!(slots, [Slot::Scalar { param: 0 }]);
}

#[test]
fn order_by_and_offset_mean_one_thing_on_both_engines() {
    let text = "select name from person order by age desc, name limit 10 offset 2";
    let (sqlite, _) = text_of(text, Backend::Sqlite, &[]);
    assert_eq!(
        sqlite,
        r#"select "name" as "name" from "person" order by "age" desc, "name" limit 10 offset 2"#
    );
    let (postgres, _) = text_of(text, Backend::Postgres, &[]);
    assert_eq!(
        postgres,
        r#"select "name" as "name" from "person" order by "age" desc nulls last, "name" nulls first limit 10::bigint offset 2::bigint"#
    );
    // An `offset` without `limit` does not parse; one built by hand has no text.
    let mut checked =
        crate::check(&SCHEMA, "select name from person limit 1 offset 2", Global).unwrap();
    let crate::ast::Statement::Select(select) = &mut checked.statement else {
        panic!("a select parsed as another statement");
    };
    select.limit = None;
    for backend in [Backend::Sqlite, Backend::Postgres] {
        let error = render(&checked, backend, &[], Global).unwrap_err();
        assert!(matches!(error, RenderError::OffsetWithoutLimit { .. }), "{error}");
    }
}

/// The oracle: the placeholders of `text` first appear numbered 1, 2, .. up to the number of
/// slots, none is past it, and each list parameter's `ListItem` slots run 0, 1, .. in order.
fn numbered_in_order(text: &[u8], slots: &[Slot], backend: Backend) -> bool {
    let mark = match backend {
        Backend::Sqlite => b'?',
        Backend::Postgres => b'$',
    };
    let mut next: u64 = 1;
    let mut at = 0;
    while at < text.len() {
        if text[at] != mark {
            at += 1;
            continue;
        }
        let digits: Vec<u8> =
            text[at + 1..].iter().copied().take_while(u8::is_ascii_digit).collect();
        if digits.is_empty() {
            return false;
        }
        let number: u64 = String::from_utf8(digits.clone()).unwrap().parse().unwrap();
        if number == next {
            next += 1;
        } else if number == 0 || number > next {
            return false;
        }
        at += 1 + digits.len();
    }
    if next - 1 != slots.len() as u64 {
        return false;
    }
    (0..8).all(|param| {
        let indices: Vec<u32> = slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::ListItem { param: p, index } if *p == param => Some(*index),
                _ => None,
            })
            .collect();
        indices.iter().copied().eq(0..indices.len() as u32)
    })
}

const PLACEHOLDERS: &str =
    "select name from person where id in $a and age > $x and id not in $b and age < $x";

/// The invariant: placeholders are numbered from 1 in the order they first appear, each with
/// one slot; a SQLite list of length n is n `ListItem` slots, a Postgres list one `Array`.
#[test]
fn placeholders_are_numbered_in_order() {
    blaze_sim::check(Config::DEFAULT, numbered_in_order_for_any_lists);
}

fn numbered_in_order_for_any_lists(tape: &Tape) {
    let a = tape.int_in(0..=5u32, "a");
    let b = tape.int_in(0..=5u32, "b");
    let backend = *tape.one_of(&[Backend::Sqlite, Backend::Postgres], "backend");
    let checked = crate::check(&SCHEMA, PLACEHOLDERS, Global).unwrap();
    let out = render(&checked, backend, &[a, b], Global).unwrap();
    assert!(
        numbered_in_order(&out.text, &out.slots, backend),
        "{:?}",
        String::from_utf8_lossy(&out.text)
    );
    let items = |param| {
        out.slots
            .iter()
            .filter(|slot| matches!(slot, Slot::ListItem { param: p, .. } if *p == param))
            .count()
    };
    match backend {
        Backend::Sqlite => assert_eq!((items(0), items(2)), (a as usize, b as usize)),
        Backend::Postgres => assert_eq!(out.slots.len(), 3),
    }
}

#[test]
fn the_oracle_catches_a_skipped_placeholder() {
    let checked = crate::check(&SCHEMA, PLACEHOLDERS, Global).unwrap();
    let out = render(&checked, Backend::Sqlite, &[1, 1], Global).unwrap();
    assert!(numbered_in_order(&out.text, &out.slots, Backend::Sqlite));
    // `?2` written as `?3`: a number skipped.
    let text = String::from_utf8(out.text.as_slice().to_vec()).unwrap().replacen("?2", "?3", 1);
    assert!(!numbered_in_order(text.as_bytes(), &out.slots, Backend::Sqlite));
    // One slot too many.
    let mut slots = out.slots.as_slice().to_vec();
    slots.push(Slot::Scalar { param: 1 });
    assert!(!numbered_in_order(&out.text, &slots, Backend::Sqlite));
}

// Stack.

/// How many operands the long chains of [`statements_at_the_bound`] have.
const LONG_CHAIN: usize = 20_000;

/// Statements whose one expression is exactly [`crate::parse::MAX_DEPTH`] deep, each typing,
/// one of each way of nesting, with their names: sums nested in parentheses (a chain in a
/// chain), nested parentheses around `-`, a `not` run over a boolean column, and `case` inside
/// `case`. Then two flat chains of [`LONG_CHAIN`] operands, 2 deep however long: `long-and`
/// (comparisons `and`-ed in a `where`) and `long-sum` (additions over an integer column), so
/// the table shows a chain's length costs no stack.
fn statements_at_the_bound() -> [(&'static str, String); 6] {
    let depth = crate::parse::MAX_DEPTH as usize - 1;
    let mut cases = String::from("active");
    let mut sums = String::from("age");
    for _ in 0..depth {
        cases = format!("case when {cases} then true end");
        sums = format!("({sums} + 1)");
    }
    [
        ("sums", format!("select {sums} as s from person")),
        (
            "neg-parens",
            format!("select {}age{} as n from person", "-(".repeat(depth), ")".repeat(depth)),
        ),
        ("nots", format!("select {}active as b from person", "not ".repeat(depth))),
        ("cases", format!("select {cases} as c from person")),
        (
            "long-and",
            format!(
                "select name from person where age = 0{}",
                " and age = 1".repeat(LONG_CHAIN - 1)
            ),
        ),
        ("long-sum", format!("select age{} as s from person", " + age".repeat(LONG_CHAIN - 1))),
    ]
}

/// The invariant: checking (parse and inference) and rendering for each engine a statement at
/// [`crate::parse::MAX_DEPTH`] each use at most [`crate::parse::STACK_MOST`] bytes of stack, as
/// `tests::stack` measures from the recursive functions' `touch` calls. It runs on a 32 MiB
/// thread so a wrong bound fails this test, naming each measurement over it, instead of
/// aborting the process. Prints `name phase bytes bytes_per_level` per measurement.
#[test]
fn inferring_and_rendering_at_the_bound_use_at_most_the_stack_bound() {
    use crate::parse::{MAX_DEPTH, STACK_MOST};
    use crate::tests::stack;
    let worker = std::thread::Builder::new()
        .name(String::from("stack-measure-infer"))
        .stack_size(32 * 1024 * 1024)
        .spawn(|| -> Result<Vec<String>, String> {
            let mut over = Vec::new();
            let mut record = |name: &str, phase: &str, bytes: usize| {
                let per_level = bytes / MAX_DEPTH as usize;
                std::eprintln!("{name} {phase} {bytes} {per_level}");
                if bytes > STACK_MOST {
                    over.push(format!("{name} {phase}: {bytes} bytes, over {STACK_MOST}"));
                }
            };
            for (name, text) in statements_at_the_bound() {
                let (checked, bytes) = stack::used(|| crate::check(&SCHEMA, &text, Global));
                record(name, "check", bytes);
                let checked =
                    checked.map_err(|error| format!("{name}: `{text}` did not check: {error}"))?;
                for (backend, phase) in
                    [(Backend::Sqlite, "render-sqlite"), (Backend::Postgres, "render-postgres")]
                {
                    let (out, bytes) = stack::used(|| render(&checked, backend, &[], Global));
                    record(name, phase, bytes);
                    out.map_err(|error| {
                        format!("{name}: `{text}` did not render for {backend:?}: {error}")
                    })?;
                }
            }
            Ok(over)
        })
        .unwrap_or_else(|error| panic!("the measuring thread did not start: {error}"));
    match worker.join() {
        Ok(Ok(over)) => assert!(over.is_empty(), "over STACK_MOST: {}", over.join("; ")),
        Ok(Err(why)) => panic!("{why}"),
        Err(_) => panic!("the measuring thread panicked"),
    }
}
