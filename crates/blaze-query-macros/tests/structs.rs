//! Inputs, interned and tracked structs, tracked `impl`s on each, `derive(Supertype)` and
//! `derive(Value)`, expanded and run on the engine.

mod common;

use blaze_query::db::{AsId, Database, Durability, FromIdIn, Id, NamesIds, Setter};
use common::{TestDb, runs};

/// The build's settings: there is one.
#[blaze_query_macros::input(singleton, debug)]
struct Settings {
    /// How deep to look.
    #[returns(copy)]
    #[get(level)]
    #[set(put_level)]
    depth: u32,
    #[default]
    #[returns(ref)]
    label: String,
    /// A field whose type names a lifetime of its own.
    #[default]
    #[returns(copy)]
    note: Option<&'static str>,
}

#[blaze_query_macros::tracked(returns(copy))]
fn deeper(db: &TestDb) -> u32 {
    Settings::get(db).level(db) + 1
}

#[test]
fn a_singleton_input_is_made_once_found_and_set() {
    let mut db = TestDb::new();
    assert_eq!(Settings::try_get(&db), None);
    let settings = Settings::builder(3)
        .durability(Durability::MEDIUM)
        .depth_durability(Durability::HIGH)
        .label("x".to_owned())
        .new(&mut db);
    assert_eq!(Settings::try_get(&db), Some(settings));
    assert_eq!(Settings::get(&db).label(&db), "x");
    assert_eq!(deeper(&db), 4);
    assert_eq!(deeper(&db), 4);

    assert_eq!(settings.put_level(&mut db).with_durability(Durability::LOW).to(9), 3);
    assert_eq!(deeper(&db), 10);
    assert_eq!(settings.set_label(&mut db).to("y".to_owned()), "x");
    assert_eq!(deeper(&db), 10);
    assert_eq!(runs(), ["deeper", "deeper"]);
}

#[test]
fn a_default_field_starts_at_its_default() {
    let mut db = TestDb::new();
    let settings = Settings::new(&mut db, 1);
    assert_eq!(settings.label(&db), "");
    assert_eq!(settings.note(&db), None);
    assert_eq!(settings.set_note(&mut db).to(Some("n")), None);
    assert_eq!(settings.note(&db), Some("n"));
}

#[test]
#[should_panic(expected = "Settings is a singleton")]
fn a_second_singleton_panics() {
    let mut db = TestDb::new();
    Settings::new(&mut db, 1);
    Settings::new(&mut db, 2);
}

#[blaze_query_macros::tracked(returns(copy))]
fn level_or_zero(db: &TestDb) -> u32 {
    Settings::try_get(db).map_or(0, |it| it.level(db))
}

/// A query that saw the singleton absent runs again once it is made.
#[test]
fn making_a_singleton_reruns_what_saw_it_absent() {
    let mut db = TestDb::new();
    assert_eq!(level_or_zero(&db), 0);
    Settings::new(&mut db, 7);
    assert_eq!(level_or_zero(&db), 7);
    assert_eq!(runs(), ["level_or_zero", "level_or_zero"]);
}

#[blaze_query_macros::input(debug)]
struct File {
    #[returns(ref)]
    text: String,
}

#[blaze_query_macros::interned(debug, constructor = make)]
struct Path<'db> {
    #[returns(ref)]
    text: String,
    #[returns(copy)]
    #[get(depth_of)]
    depth: u32,
}

#[blaze_query_macros::interned(unsafe(no_lifetime))]
struct Symbol {
    #[returns(deref)]
    text: Box<str>,
}

#[test]
fn equal_fields_intern_to_one_id_by_any_borrowed_form() {
    let db = TestDb::new();
    let a = Path::make(&db, "a", 1u32);
    assert_eq!(a, Path::make(&db, "a".to_owned(), 1u32));
    assert_ne!(a, Path::make(&db, "a", 2u32));
    assert_eq!(a.text(&db), "a");
    assert_eq!(a.depth_of(&db), 1);

    let s = Symbol::new(&db, "s");
    assert!(s == Symbol::new(&db, Box::<str>::from("s")), "one id by any borrowed form");
    let text: &str = s.text(&db);
    assert_eq!(text, "s");

    let shown = blaze_query::db::attach(&db, || format!("{a:?}"));
    assert_eq!(shown, r#"Path { text: "a", depth: 1 }"#);
}

/// IDs an interned field names besides being one.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Members(Vec<Id>);

impl NamesIds for Members {
    fn names_ids(&self, out: &mut Vec<Id>) {
        out.extend_from_slice(&self.0);
    }
}

#[blaze_query_macros::interned]
struct Group<'db> {
    #[returns(copy)]
    lead: Path<'db>,
    #[returns(ref)]
    members: Members,
    #[returns(copy)]
    size: u32,
}

#[test]
fn an_interned_row_names_the_ids_of_its_fields() {
    let db = TestDb::new();
    let lead = Path::make(&db, "lead", 0u32);
    let file = File::new(&db, String::new());
    let symbol = Symbol::new(&db, "x");
    let group = Group::new(&db, lead, Members(vec![file.as_id(), symbol.as_id()]), 2u32);
    assert_eq!(group.lead(&db), lead);
    assert_eq!(group.size(&db), 2);
    assert_eq!(group.members(&db).0.len(), 2);

    let mut out = Vec::new();
    db.storage().ids_of(group.as_id(), &mut out);
    assert_eq!(out, [lead.as_id(), file.as_id(), symbol.as_id()]);
}

#[blaze_query_macros::tracked(debug)]
struct Parsed<'db> {
    #[returns(copy)]
    origin: Path<'db>,
    #[tracked]
    #[returns(ref)]
    words: Vec<String>,
    #[tracked]
    #[returns(copy)]
    count: usize,
    #[tracked]
    #[no_eq]
    #[returns(copy)]
    stamp: u32,
}

#[blaze_query_macros::tracked(returns(copy))]
fn parse(db: &TestDb, file: File) -> Parsed<'_> {
    let origin = Path::make(db, "parsed", 0u32);
    let words: Vec<String> = file.text(db).split(' ').map(str::to_owned).collect();
    let count = words.len();
    Parsed::new(db, origin, words, count, 0)
}

#[blaze_query_macros::tracked(returns(copy))]
fn word_count(db: &TestDb, file: File) -> usize {
    parse(db, file).count(db)
}

#[blaze_query_macros::tracked(returns(clone))]
fn first_word(db: &TestDb, file: File) -> String {
    parse(db, file).words(db)[0].clone()
}

#[blaze_query_macros::tracked(returns(copy))]
fn stamp_reader(db: &TestDb, file: File) -> u32 {
    parse(db, file).stamp(db)
}

#[test]
fn a_tracked_struct_keeps_its_id_and_each_field_changes_alone() {
    let mut db = TestDb::new();
    let file = File::new(&db, "a b".to_owned());
    let parsed = parse(&db, file);
    assert_eq!(parsed.origin(&db).text(&db), "parsed");
    assert_eq!(
        (word_count(&db, file), first_word(&db, file), stamp_reader(&db, file)),
        (2, "a".to_owned(), 0)
    );
    let mut ran = runs();
    ran.sort_unstable();
    assert_eq!(ran, ["first_word", "parse", "stamp_reader", "word_count"]);

    let shown = blaze_query::db::attach(&db, || format!("{parsed:?}"));
    assert!(shown.starts_with("Parsed { [id]: "), "{shown}");
    assert!(shown.ends_with(r#"words: ["a", "b"], count: 2, stamp: 0 }"#), "{shown}");
    let id = parsed.as_id();

    file.set_text(&mut db).to("c d".to_owned());
    assert_eq!(parse(&db, file).as_id(), id, "the same identity in the same query");
    assert_eq!(
        (word_count(&db, file), first_word(&db, file), stamp_reader(&db, file)),
        (2, "c".to_owned(), 0)
    );
    let mut ran = runs();
    ran.sort_unstable();
    assert_eq!(
        ran,
        ["first_word", "parse", "stamp_reader"],
        "`count` is equal, `stamp` is `no_eq`"
    );
}

#[test]
fn a_field_is_named_by_its_ident_and_a_tracked_struct_names_its_tracked_fields() {
    let mut db = TestDb::new();
    let settings = Settings::builder(3).new(&mut db);
    let file = File::new(&db, "a b".to_owned());
    let parsed = parse(&db, file);
    let storage = db.storage();
    let names = |id: Id, count| {
        let ingredient = storage.owner(id).expect("a made row");
        (0..count).map(|field| storage.field_name(ingredient, field)).collect::<Vec<_>>()
    };
    assert_eq!(names(settings.as_id(), 3), ["depth", "label", "note"], "not the getter `level`");
    assert_eq!(
        names(parsed.as_id(), 3),
        ["words", "count", "stamp"],
        "not the identity's `origin`"
    );
}

#[blaze_query_macros::tracked]
impl<'db> Parsed<'db> {
    /// The word at `index`.
    #[blaze_query::tracked(returns(as_ref))]
    fn word(self, db: &'db TestDb, index: usize) -> Option<String> {
        self.words(db).get(index).cloned()
    }

    #[blaze_query::tracked(returns(copy))]
    fn total(self, db: &'db TestDb) -> usize {
        self.count(db) + self.offset()
    }

    fn offset(self) -> usize {
        0
    }
}

#[blaze_query_macros::tracked]
impl<'db> Path<'db> {
    #[blaze_query::tracked(returns(copy))]
    fn len(self, db: &'db TestDb) -> usize {
        self.text(db).len()
    }

    #[blaze_query::tracked(returns(copy))]
    fn root_depth(db: &'db TestDb) -> u32 {
        Self::make(db, "root", 7u32).depth_of(db)
    }
}

#[blaze_query_macros::tracked]
impl File {
    #[blaze_query::tracked(returns(copy))]
    fn size(self, db: &TestDb) -> usize {
        self.text(db).len()
    }
}

#[test]
fn a_tracked_impl_makes_queries_of_its_methods() {
    let db = TestDb::new();
    let file = File::new(&db, "ab cd".to_owned());
    let parsed = parse(&db, file);
    assert_eq!(parsed.word(&db, 1).map(String::as_str), Some("cd"));
    assert_eq!(parsed.word(&db, 2), None);
    assert_eq!(parsed.total(&db), 2);
    assert_eq!(Path::make(&db, "abc", 0u32).len(&db), 3);
    assert_eq!(Path::root_depth(&db), 7);
    assert_eq!(file.size(&db), 5);
    assert_eq!(file.size(&db), 5);
    let ran = runs();
    assert_eq!(ran.iter().filter(|it| it.ends_with("::word")).count(), 2, "{ran:?}");
    assert!(ran.iter().any(|it| it.ends_with("::root_depth")), "{ran:?}");
    assert_eq!(ran.iter().filter(|it| **it == "File::size").count(), 1, "{ran:?}");
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, blaze_query_macros::Supertype)]
enum Node<'db> {
    Path(Path<'db>),
    File(File),
}

#[blaze_query_macros::tracked(returns(copy))]
fn node_size<'db>(db: &'db TestDb, node: Node<'db>) -> usize {
    match node {
        Node::Path(path) => path.len(db),
        Node::File(file) => file.size(db),
    }
}

#[test]
fn a_supertype_is_an_id_whose_variant_its_page_owner_picks() {
    let db = TestDb::new();
    let path = Path::make(&db, "p", 0u32);
    let file = File::new(&db, "file".to_owned());
    let symbol = Symbol::new(&db, "s");
    let storage = db.storage();
    assert_eq!(Node::from_id_in(storage, path.as_id()), Some(Node::Path(path)));
    assert_eq!(Node::from_id_in(storage, file.as_id()), Some(Node::File(file)));
    assert_eq!(Node::from_id_in(storage, symbol.as_id()), None);
    assert_eq!(Path::from_id_in(storage, file.as_id()), None);
    assert_eq!(Node::File(file).as_id(), file.as_id());
    assert_eq!(node_size(&db, Node::Path(path)), 1);
    assert_eq!(node_size(&db, Node::File(file)), 4);
}

#[derive(Clone, PartialEq, Eq, Debug, blaze_query_macros::Value)]
struct Listing<'db> {
    paths: Vec<Path<'db>>,
    total: usize,
}

#[derive(Clone, PartialEq, Eq, Debug, blaze_query_macros::Value)]
enum Choice<'db> {
    One(Path<'db>),
    Nothing,
}

/// A path the derive can't see through, vouched for by hand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Opaque<'db>(Path<'db>);

#[derive(Clone, PartialEq, Eq, Debug, blaze_query_macros::Value)]
struct Vouched<'db> {
    #[value(unsafe(prove_safe_to_retain_manually))]
    by_hand: Opaque<'db>,
    #[value(unsafe(prove(Path<'db>: blaze_query::db::Value)))]
    by_bound: Opaque<'db>,
}

#[derive(Clone, PartialEq, Eq, Debug, blaze_query_macros::Value)]
struct Wrapped<T> {
    inner: T,
}

#[blaze_query_macros::tracked(returns(ref))]
fn listing<'db>(db: &'db TestDb, file: File) -> Listing<'db> {
    let paths: Vec<Path<'db>> =
        file.text(db).split(' ').map(|it| Path::make(db, it, 1u32)).collect();
    Listing { total: paths.len(), paths }
}

#[blaze_query_macros::tracked(returns(clone))]
fn choice<'db>(db: &'db TestDb, file: File) -> Choice<'db> {
    match listing(db, file).paths.first() {
        Some(&path) if !path.text(db).is_empty() => Choice::One(path),
        _ => Choice::Nothing,
    }
}

#[blaze_query_macros::tracked(returns(clone))]
fn vouched<'db>(db: &'db TestDb, file: File) -> Vouched<'db> {
    let path = listing(db, file).paths[0];
    Vouched { by_hand: Opaque(path), by_bound: Opaque(path) }
}

#[blaze_query_macros::tracked(returns(clone))]
fn wrapped<'db>(db: &'db TestDb, file: File) -> Wrapped<Path<'db>> {
    Wrapped { inner: listing(db, file).paths[0] }
}

#[test]
fn derived_values_name_the_database_lifetime() {
    let db = TestDb::new();
    let file = File::new(&db, "x y".to_owned());
    let empty = File::new(&db, String::new());
    let x = Path::make(&db, "x", 1u32);
    assert_eq!(listing(&db, file).total, 2);
    assert_eq!(listing(&db, file).paths[0], x);
    assert_eq!(choice(&db, file), Choice::One(x));
    assert_eq!(choice(&db, empty), Choice::Nothing);
    assert_eq!(vouched(&db, file), Vouched { by_hand: Opaque(x), by_bound: Opaque(x) });
    assert_eq!(wrapped(&db, file), Wrapped { inner: x });
}
