//! Tracked functions expanded and run on the engine: no, one and several arguments, every return
//! mode, `lru`, the three ways a cycle goes, `no_eq`, `self_ty`, `mem_tag`, the database
//! lifetime, and `Debug` through the attached database.

mod common;

use std::cell::Cell;

use blaze_query::db::{AsId, Cycle, Database, Durability, Id, Setter};
use common::{TestDb, keyed_runs, runs};

#[blaze_query_macros::input(debug)]
struct Source {
    #[returns(ref)]
    text: String,
    #[returns(copy)]
    weight: u32,
}

#[blaze_query_macros::interned(debug)]
struct Name<'db> {
    #[returns(ref)]
    text: String,
}

/// The answer, a query of no arguments.
#[blaze_query_macros::tracked(returns(copy))]
fn answer(_db: &TestDb) -> u32 {
    42
}

#[blaze_query_macros::tracked(returns(copy))]
fn length(db: &TestDb, source: Source) -> usize {
    source.text(db).len()
}

#[test]
fn a_query_runs_once_and_again_when_what_it_read_changes() {
    let mut db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, "a,b".to_owned(), 2);
    assert_eq!(answer(&db), 42);
    assert_eq!(answer(&db), 42);
    assert_eq!(runs(), ["answer"]);

    assert_eq!(length(&db, source), 3);
    assert_eq!(length(&db, source), 3);
    assert_eq!(runs(), ["length"]);

    assert_eq!(source.set_weight(&mut db).to(3), 2);
    assert_eq!(length(&db, source), 3);
    assert!(runs().is_empty(), "`length` reads no `weight`");

    assert_eq!(source.set_text(&mut db).to("a,b,c".to_owned()), "a,b");
    assert_eq!(length(&db, source), 5);
    assert_eq!(answer(&db), 42);
    assert_eq!(runs(), ["length"]);
}

#[blaze_query_macros::tracked(returns(clone))]
fn repeat(db: &TestDb, source: Source, times: usize, separator: char) -> String {
    let text = source.text(db);
    let mut out = String::new();
    for i in 0..times {
        if i > 0 {
            out.push(separator);
        }
        out.push_str(text);
    }
    out
}

#[test]
fn several_arguments_are_one_hidden_key() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, "ab".to_owned(), 1);
    assert_eq!(repeat(&db, source, 3, '-'), "ab-ab-ab");
    assert_eq!(repeat(&db, source, 3, '-'), "ab-ab-ab");
    assert_eq!(repeat(&db, source, 2, '+'), "ab+ab");
    let ran = keyed_runs();
    assert_eq!(ran.len(), 2, "{ran:?}");
    assert!(ran.iter().all(|(name, _)| *name == "repeat"), "{ran:?}");
    assert_ne!(ran[0].1, ran[1].1);
    assert_eq!(repeat::arguments(&db, ran[0].1), Some((source, 3, '-')));
    assert_eq!(repeat::arguments(&db, ran[1].1), Some((source, 2, '+')));
    assert_eq!(repeat::arguments(&db, source.as_id()), None);
}

#[blaze_query_macros::tracked(returns(ref))]
fn words(db: &TestDb, source: Source) -> Vec<String> {
    source.text(db).split(',').map(str::to_owned).collect()
}

#[blaze_query_macros::tracked(returns(deref))]
fn boxed(db: &TestDb, source: Source) -> Box<str> {
    source.text(db).as_str().into()
}

#[blaze_query_macros::tracked(returns(as_ref))]
fn first(db: &TestDb, source: Source) -> Option<String> {
    words(db, source).first().cloned()
}

#[blaze_query_macros::tracked(returns(as_ref))]
fn parsed(db: &TestDb, source: Source) -> Result<u32, String> {
    source.text(db).parse().map_err(|_| source.text(db).clone())
}

#[blaze_query_macros::tracked(returns(as_deref))]
fn first_str(db: &TestDb, source: Source) -> Option<Box<str>> {
    first(db, source).map(|it| it.as_str().into())
}

#[test]
fn every_return_mode_hands_out_its_form() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, "x,y".to_owned(), 1);
    let number = Source::new(&db, Durability::LOW, "12".to_owned(), 1);

    let all: &Vec<String> = words(&db, source);
    assert_eq!(all, &["x", "y"]);
    assert!(std::ptr::eq(all, words(&db, source)), "`ref` hands out the row's value");
    let text: &str = boxed(&db, source);
    assert_eq!(text, "x,y");
    let head: Option<&String> = first(&db, source);
    assert_eq!(head.map(String::as_str), Some("x"));
    let head: Option<&str> = first_str(&db, source);
    assert_eq!(head, Some("x"));
    let ok: Result<&u32, &String> = parsed(&db, number);
    assert_eq!(ok, Ok(&12));
    assert_eq!(parsed(&db, source), Err(&"x,y".to_owned()));
}

#[blaze_query_macros::tracked(returns(copy), lru = 0)]
fn doubled(db: &TestDb, source: Source) -> u32 {
    source.weight(db) * 2
}

#[test]
fn an_lru_query_evicts_its_values_and_runs_again() {
    let mut db = TestDb::new();
    let a = Source::new(&db, Durability::LOW, String::new(), 1);
    let b = Source::new(&db, Durability::LOW, String::new(), 2);
    assert_eq!((doubled(&db, a), doubled(&db, b)), (2, 4));
    assert_eq!(runs(), ["doubled", "doubled"]);

    assert_eq!(doubled::evict_values_of(&mut db, [a]), 1);
    assert_eq!(doubled(&db, b), 4);
    assert!(runs().is_empty());
    assert_eq!(doubled(&db, a), 2);
    assert_eq!(runs(), ["doubled"]);

    assert_eq!(doubled::evict_values(&mut db), 2);
    assert_eq!(doubled::evict_values(&mut db), 0);
    assert_eq!((doubled(&db, a), doubled(&db, b)), (2, 4));
    assert_eq!(runs(), ["doubled", "doubled"]);
}

/// Weighs its value at its source's weight in bytes.
#[blaze_query_macros::tracked(returns(copy), lru, weigh = |it: &u32| *it as usize / 3, mem_tag = Usercode)]
fn tripled(db: &TestDb, source: Source) -> u32 {
    source.weight(db) * 3
}

#[test]
fn an_lru_query_keeps_its_values_under_its_tags_budget_from_the_next_view() {
    use blaze_query::db::plumbing::Tag;
    let mut db = TestDb::new();
    let sources: Vec<Source> =
        (1..=6).map(|i| Source::new(&db, Durability::LOW, String::new(), i)).collect();
    let fetch_all = |db: &TestDb| {
        for (&source, weight) in sources.iter().zip(1..) {
            assert_eq!(tripled(db, source), weight * 3);
        }
    };
    // 1 + 2 + .. + 6 = 21 bytes against 16: the pass lets values go down to 14, the oldest
    // first (of one view, by key): after 1, 2 and 3, 15 bytes are still over, so 4 goes too.
    db.storage_mut().set_value_budget(Tag::Usercode, Some(16));
    fetch_all(&db);
    assert_eq!(runs().len(), 6);
    db.storage_mut().synthetic_write(Durability::LOW);
    fetch_all(&db);
    assert_eq!(runs(), ["tripled"; 4], "5 + 6 = 11 bytes kept, 1 to 4 run again");

    db.storage_mut().set_value_budget(Tag::Usercode, Some(u64::MAX));
    db.storage_mut().synthetic_write(Durability::LOW);
    fetch_all(&db);
    assert!(runs().is_empty(), "no budget: nothing evicted");
}

#[blaze_query_macros::tracked(returns(copy), mem_tag = Infer)]
fn tagged(db: &TestDb, source: Source) -> u32 {
    source.weight(db) + 1
}

#[blaze_query_macros::tracked(returns(copy), self_ty = Source)]
fn weighed(db: &TestDb, source: Source) -> u32 {
    source.weight(db)
}

#[test]
fn mem_tag_and_self_ty_expand_and_self_ty_names_the_query() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, String::new(), 5);
    assert_eq!(tagged(&db, source), 6);
    assert_eq!(weighed(&db, source), 5);
    assert_eq!(runs(), ["tagged", "Source::weighed"]);
}

#[blaze_query_macros::tracked(returns(copy), cycle_result = looped_result)]
fn looped(db: &TestDb, source: Source) -> u32 {
    looped(db, source) + 1
}

fn looped_result(_db: &TestDb, _id: Id, _source: Source) -> u32 {
    100
}

#[test]
fn a_cycle_result_is_the_value_of_the_cycle() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, String::new(), 1);
    assert_eq!(looped(&db, source), 100);
    assert_eq!(looped(&db, source), 100);
}

thread_local! {
    static ROUNDS: Cell<u32> = const { Cell::new(0) };
}

#[blaze_query_macros::tracked(
    returns(copy),
    cycle_initial = climb_initial,
    cycle_fn = climb_recover,
)]
fn climb(db: &TestDb, source: Source) -> u32 {
    (climb(db, source) + 1).min(source.weight(db))
}

fn climb_initial(_db: &TestDb, _id: Id, _source: Source) -> u32 {
    0
}

fn climb_recover(_db: &TestDb, cycle: &Cycle<'_>, last: &u32, value: u32, source: Source) -> u32 {
    assert_eq!(cycle.id(), source.as_id());
    assert!(value >= *last, "{value} after {last}");
    ROUNDS.with(|rounds| rounds.set(rounds.get().max(cycle.iteration())));
    value
}

#[test]
fn a_fixpoint_runs_from_its_initial_value_until_it_settles() {
    let mut db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, String::new(), 3);
    assert_eq!(climb(&db, source), 3);
    assert!(ROUNDS.with(Cell::get) > 1, "it took rounds");
    source.set_weight(&mut db).to(5);
    assert_eq!(climb(&db, source), 5);
}

#[blaze_query_macros::tracked(returns(copy), cycle_initial = |_, _, _| 0)]
fn settle(db: &TestDb, source: Source) -> u32 {
    (settle(db, source) + 2).min(source.weight(db) * 2)
}

#[test]
fn a_closure_is_a_cycle_initial_and_no_cycle_fn_keeps_each_round() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, String::new(), 3);
    assert_eq!(settle(&db, source), 6);
}

#[blaze_query_macros::tracked(returns(copy))]
fn spin(db: &TestDb, source: Source) -> u32 {
    spin(db, source)
}

#[test]
#[should_panic(expected = "dependency graph cycle")]
fn a_cycle_without_recovery_panics() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, String::new(), 1);
    spin(&db, source);
}

#[blaze_query_macros::tracked(returns(copy), no_eq)]
fn even_no_eq(db: &TestDb, source: Source) -> bool {
    source.weight(db) % 2 == 0
}

#[blaze_query_macros::tracked(returns(copy))]
fn even_no_eq_reader(db: &TestDb, source: Source) -> u8 {
    u8::from(even_no_eq(db, source))
}

#[blaze_query_macros::tracked(returns(copy))]
fn even(db: &TestDb, source: Source) -> bool {
    source.weight(db) % 2 == 0
}

#[blaze_query_macros::tracked(returns(copy))]
fn even_reader(db: &TestDb, source: Source) -> u8 {
    u8::from(even(db, source))
}

#[test]
fn no_eq_makes_every_run_a_change_and_equal_values_cut_off() {
    let mut db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, String::new(), 2);
    assert_eq!((even_no_eq_reader(&db, source), even_reader(&db, source)), (1, 1));
    let mut ran = runs();
    ran.sort_unstable();
    assert_eq!(ran, ["even", "even_no_eq", "even_no_eq_reader", "even_reader"]);

    source.set_weight(&mut db).to(4);
    assert_eq!((even_no_eq_reader(&db, source), even_reader(&db, source)), (1, 1));
    let mut ran = runs();
    ran.sort_unstable();
    assert_eq!(ran, ["even", "even_no_eq", "even_no_eq_reader"]);
}

/// The names of a source's comma-separated words.
#[blaze_query_macros::tracked(returns(ref))]
pub(crate) fn names<'db>(db: &'db TestDb, source: Source) -> Vec<Name<'db>> {
    source.text(db).split(',').map(|it| Name::new(db, it)).collect()
}

#[blaze_query_macros::tracked(returns(clone))]
fn joined<'db>(db: &'db TestDb, left: Name<'db>, right: Name<'db>) -> String {
    format!("{}{}", left.text(db), right.text(db))
}

#[blaze_query_macros::tracked(returns(copy))]
fn elided(db: &TestDb, name: Name<'_>) -> usize {
    name.text(db).len()
}

#[test]
fn keys_and_values_name_the_database_lifetime() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, "ab,c,ab".to_owned(), 1);
    let all = names(&db, source);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0], all[2], "equal fields intern to one ID");
    assert_ne!(all[0], all[1]);
    assert_eq!(all[0], Name::new(&db, "ab".to_owned()));
    assert_eq!(joined(&db, all[0], all[1]), "abc");
    assert_eq!(elided(&db, all[1]), 1);
}

#[test]
fn debug_reads_the_attached_database() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, "x".to_owned(), 7);
    let name = Name::new(&db, "n");

    let detached = format!("{source:?}");
    assert_eq!(detached, format!("Source {{ [id]: {:?} }}", source.as_id()));
    assert_eq!(format!("{name:?}"), format!("Name({:?})", name.as_id()));

    let attached = blaze_query::db::attach(&db, || format!("{source:?} {name:?}"));
    let id = source.as_id();
    assert_eq!(
        attached,
        format!(r#"Source {{ [id]: {id:?}, text: "x", weight: 7 }} Name {{ text: "n" }}"#)
    );
}

/// A bound that trips names the key's value, not its number (rule 19): an argument's `Debug`
/// through the database the walk ran on, each argument of a key of several, and `()` for none.
#[test]
fn out_of_fuel_names_the_keys_value() {
    let db = TestDb::new();
    let source = Source::new(&db, Durability::LOW, "ab".to_owned(), 1);
    let id = source.as_id();

    let out = blaze_query::db::with_fuel(0, || length(&db, source)).expect_err("no fuel");
    assert_eq!((out.steps, out.query), (0, "length"), "{out}");
    assert_eq!(out.value, format!(r#"Source {{ [id]: {id:?}, text: "ab", weight: 1 }}"#), "{out}");
    assert!(out.to_string().contains(r#"text: "ab""#), "{out}");

    let out = blaze_query::db::with_fuel(0, || repeat(&db, source, 3, '-')).expect_err("no fuel");
    assert_eq!(
        out.value,
        format!(r#"(Source {{ [id]: {id:?}, text: "ab", weight: 1 }}, 3, '-')"#),
        "{out}"
    );

    let out = blaze_query::db::with_fuel(0, || answer(&db)).expect_err("no fuel");
    assert_eq!(out.value, "()", "{out}");

    let ran = blaze_query::db::with_fuel(1 << 10, || length(&db, source));
    assert!(matches!(ran, Ok((2, 1..))), "enough fuel ends, and says what it spent: {ran:?}");
}
