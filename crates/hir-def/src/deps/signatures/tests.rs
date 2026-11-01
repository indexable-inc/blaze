use parking_lot::Mutex;

use blaze_format::Aligned;
use blaze_hash::{HashMap, HashSet};
use hir_expand::fixture;

use super::*;
use crate::{
    deps::{DepImages, ImageRef, ImageStore, keys, serve, store, write_image},
    test_db::{TestDB, TestDBExt},
};

/// Every kind of signature, with generics, where clauses, defaults, cfg'd-out fields and
/// variants, items a `macro_rules!` writes, derives, a `#[path]` module and an `include!`d file.
const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
fn main() {}

//- /lib.rs crate:lib deps:core new_source_root:library
#[path = "elsewhere/placed.rs"]
pub mod placed;
mod made;
#[rustc_builtin_macro]
macro_rules! include { () => {} }
include!("included.rs");

pub struct Unit;
pub struct Tuple<T>(pub T, u8);
pub struct Record<'a, T: Clone + 'a, const N: usize = 3>
where
    T: Copy,
{
    pub slice: &'a [T; N],
    pub(crate) maybe: Option<T>,
    #[cfg(any())]
    gone: u8,
}
pub union Bits { pub int: u32, pub float: f32 }
#[repr(u8)]
pub enum Kind<T> { A, B(T) = 4, C { x: u8 }, #[cfg(any())] D }
pub const LIMIT: usize = 3;
pub static NAME: &str = "lib";
pub type Pair<T, U = T> = (T, U);
pub trait Shape<T = ()>: Sized {
    type Out: Clone;
    const SIDES: u8;
    fn area(&self, by: T) -> Self::Out;
    fn new() -> Self where Self: Sized;
}
impl<'a, T: Clone + Copy> Shape<T> for Record<'a, T, 2> where T: Copy {
    type Out = T;
    const SIDES: u8 = 4;
    fn area(&self, by: T) -> T { by }
    fn new() -> Self { loop {} }
}
impl Unit {
    pub const fn make() -> Self { Unit }
    pub unsafe extern "C" fn raw(_: *const u8) {}
    pub fn by_ref(&self, f: &dyn Fn(u8) -> u8, g: impl Clone) -> impl Clone { g }
}
extern "C" {
    pub fn outside(x: i32) -> i32;
    pub static OUTSIDE: u8;
}
const _: () = ();
macro_rules! make {
    ($name:ident, $ty:ty) => {
        pub fn $name<T: Default>(x: $ty) -> T { T::default() }
        pub struct Made(pub $ty);
    };
}
make!(made_fn, u32);
#[derive(Clone, Debug)]
pub struct Derived { pub x: u8 }
pub fn lifetimes<'a, 'b: 'a>(x: &'a str, _: &'b str) -> &'a str where 'b: 'a { x }
pub fn f() -> u8 { 0 }

//- /made.rs
pub fn from_file(x: crate::Unit) -> crate::Unit { x }

//- /elsewhere/placed.rs
pub struct Placed { pub at: [u8; 4] }

//- /included.rs
pub fn from_include(x: u8) -> u8 { x }
pub struct Included<T>(T);

//- /core.rs crate:core new_source_root:library
#[lang = "sized"]
pub trait Sized {}
pub trait Clone { fn clone(&self) -> Self; }
#[lang = "copy"]
pub trait Copy: Clone {}
pub trait Default { fn default() -> Self; }
#[lang = "fn"]
pub trait Fn<Args> {}
pub mod option { pub enum Option<T> { None, Some(T) } }
pub mod fmt { pub trait Debug {} }
#[rustc_builtin_macro]
pub macro derive {}
#[rustc_builtin_macro]
pub macro Clone {}
#[rustc_builtin_macro]
pub macro Debug {}
pub mod prelude {
    pub mod rust_2018 {
        pub use crate::{Clone, Copy, Debug, Default, Fn, Sized, derive, option::Option};
    }
    pub mod rust_2021 { pub use super::rust_2018::*; }
}
"#;

/// Const arguments that are blocks with items, one in another: their blocks are made when the
/// signature is read.
const BLOCKS: &str = r#"
//- /lib.rs crate:lib new_source_root:library
pub struct Arr<const N: usize>([u8; N]);
pub struct Blocky(pub [u8; { const A: usize = 1; A + 1 }]);
pub fn nested() -> Arr<{ const A: usize = 1; { const B: usize = A; B } }> { loop {} }
pub type Alias = [u8; { struct Q; 2 }];
"#;

const BUILD: ContentId = ContentId([7; 32]);

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

fn all(db: &TestDB) -> Vec<Crate> {
    base_db::all_crates(db).iter().copied().collect()
}

/// A store in memory.
#[derive(Default)]
struct Memory {
    images: Mutex<HashMap<ContentId, std::sync::Arc<Aligned>>>,
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<crate::deps::ImageBuf>, String> {
        Ok(self.images.lock().get(key).map(|it| it.clone() as crate::deps::ImageBuf))
    }

    fn put(&self, key: ContentId, image: Vec<u8>) -> Result<(), String> {
        let image = std::sync::Arc::new(Aligned::copy_from(&image));
        self.images.lock().entry(key).or_insert(image);
        Ok(())
    }

    fn reads_now(&self, _: &base_db::Env, _: &[u8]) -> Result<ContentId, String> {
        Err("these fixtures expand no proc macro".to_owned())
    }
}

/// A store with the images of a fresh database of `fixture`, every dependency written.
fn stored(fixture: &str) -> Memory {
    let mut db = fixture::db(fixture).into_db();
    let memory = Memory::default();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let stored = store(&db, &served, &memory).unwrap();
    assert_eq!(stored.refused, Vec::new());
    assert!(!stored.written.is_empty());
    memory
}

/// A fresh database of `fixture` reading every dependency from `memory`, in `mode`.
fn served_db(fixture: &str, memory: &Memory, mode: Mode) -> TestDB {
    let mut db = fixture::db(fixture).into_db();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, memory, mode).unwrap();
    assert!(served.hits.contains(&crate_named(&db, "lib")));
    db
}

/// The differences frozen bytes record are the process's: the tests that read them take turns
/// with the other image tests.
fn turn() -> parking_lot::MutexGuard<'static, ()> {
    crate::deps::tests::TURN.lock()
}

/// Asserts that `item`'s signature as `db` reads it from its image is the one `db` lowers from
/// syntax: the same record and, with `values`, the same value, read by the `served_*` function
/// itself. A block is a tracked struct of the query that makes it, another id in each, so a
/// signature with one compares by its record, read through its query.
fn assert_served(db: &TestDB, krate: Crate, item: Item, values: bool) {
    let what = item.what();
    macro_rules! same {
        ($code:ident, $sig:ident, $served:ident, $id:expr) => {{
            let direct =
                values.then(|| $served(db, $id).expect("an item of a crate with an image"));
            let served: &$sig = match &direct {
                Some(it) => it,
                None => $sig::of(db, $id),
            };
            let lowered: &$sig = &$sig::with_source_map(db, $id).0;
            let records = (
                record::<codes::$code>(db, krate, what, served),
                record::<codes::$code>(db, krate, what, lowered),
            );
            assert!(records.0.is_ok(), "{item:?}: {records:?}");
            assert_eq!(records.0, records.1, "{item:?}");
            if values {
                assert_eq!(served, lowered, "{item:?}");
            }
        }};
    }
    match item {
        Item::Struct(id) => same!(Struct, StructSignature, served_struct, id),
        Item::Union(id) => same!(Union, UnionSignature, served_union, id),
        Item::Enum(id) => same!(Enum, EnumSignature, served_enum, id),
        Item::Const(id) => same!(Const, ConstSignature, served_const, id),
        Item::Static(id) => same!(Static, StaticSignature, served_static, id),
        Item::Impl(id) => same!(Impl, ImplSignature, served_impl, id),
        Item::Trait(id) => same!(Trait, TraitSignature, served_trait, id),
        Item::Function(id) => same!(Function, FunctionSignature, served_function, id),
        Item::TypeAlias(id) => same!(TypeAlias, TypeAliasSignature, served_type_alias, id),
        Item::Fields(id) => same!(Fields, VariantFields, served_variant_fields, id),
        Item::Variants(id) => {
            // Outside the query, which runs in a frame of its `mem_tag`, the lowering needs one.
            let in_frame = |f: &dyn Fn() -> (EnumVariants, ThinVec<InactiveEnumVariantCode>)| {
                blaze_mem::frame::task(blaze_mem::Tag::Nameres, |_| f())
            };
            let direct = values.then(|| {
                in_frame(&|| served_enum_variants(db, id).expect("an enum with an image"))
            });
            let served = match &direct {
                Some(it) => it,
                None => EnumVariants::of(db, id),
            };
            let lowered = in_frame(&|| EnumVariants::lower(db, id));
            let records = (
                record::<codes::Variants>(db, krate, what, served),
                record::<codes::Variants>(db, krate, what, &lowered),
            );
            assert!(records.0.is_ok(), "{item:?}: {records:?}");
            assert_eq!(records.0, records.1, "{item:?}");
            if values {
                assert_eq!(*served, lowered, "{item:?}");
            }
        }
    }
}

#[test]
fn every_served_signature_is_the_lowered_one() {
    let _turn = turn();
    let memory = stored(FIXTURE);
    let db = served_db(FIXTURE, &memory, Mode::Serve);
    for name in ["lib", "core"] {
        let krate = crate_named(&db, name);
        let items = items(&db, krate);
        if name == "lib" {
            let kinds: HashSet<u8> = items.iter().map(|it| it.what()).collect();
            assert_eq!(kinds.len(), 11, "the fixture asks every query: {kinds:?}");
        }
        for item in items {
            assert_served(&db, krate, item, true);
        }
    }
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

#[test]
fn every_signature_is_served_through_its_query() {
    let _turn = turn();
    let memory = stored(FIXTURE);
    let db = served_db(FIXTURE, &memory, Mode::Serve);
    let krate = crate_named(&db, "lib");
    for item in items(&db, krate) {
        assert_served(&db, krate, item, false);
    }
}

#[test]
fn signatures_with_blocks_read_back() {
    let _turn = turn();
    let memory = stored(BLOCKS);
    for mode in [Mode::Serve, Mode::Verify, Mode::Check] {
        let db = served_db(BLOCKS, &memory, mode);
        let krate = crate_named(&db, "lib");
        for item in items(&db, krate) {
            assert_served(&db, krate, item, false);
        }
        assert_eq!(frozen::differences(), Vec::<String>::new(), "{mode:?}");
    }
}

#[test]
fn the_part_is_the_same_bytes_in_every_database() {
    for fixture in [FIXTURE, BLOCKS] {
        let part = || {
            let db = fixture::db(fixture).into_db();
            encode(&db, crate_named(&db, "lib"), &mut Vec::new())
        };
        let first = part();
        assert!(first.is_ok(), "{first:?}");
        assert_eq!(first, part());
    }
}

/// The items `db` serves without asking for its trait's or impl's items, which parse them:
/// what the def map declares, its impls and its enums' variants.
fn declared(db: &TestDB, krate: Crate) -> Vec<Item> {
    let mut found = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => found.push(Item::Function(it)),
                ModuleDefId::AdtId(AdtId::StructId(it)) => {
                    found.extend([Item::Struct(it), Item::Fields(VariantId::StructId(it))]);
                }
                ModuleDefId::AdtId(AdtId::UnionId(it)) => found.push(Item::Union(it)),
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    found.extend([Item::Enum(it), Item::Variants(it)]);
                }
                ModuleDefId::ConstId(it) => found.push(Item::Const(it)),
                ModuleDefId::StaticId(it) => found.push(Item::Static(it)),
                ModuleDefId::TraitId(it) => found.push(Item::Trait(it)),
                ModuleDefId::TypeAliasId(it) => found.push(Item::TypeAlias(it)),
                _ => {}
            }
        }
        found.extend(scope.impls().map(Item::Impl));
    }
    found
}

/// Reads `item`'s signature from its image, through the `served_*` function.
fn serve_one(db: &TestDB, item: Item) -> bool {
    match item {
        Item::Struct(id) => served_struct(db, id).is_some(),
        Item::Union(id) => served_union(db, id).is_some(),
        Item::Enum(id) => served_enum(db, id).is_some(),
        Item::Const(id) => served_const(db, id).is_some(),
        Item::Static(id) => served_static(db, id).is_some(),
        Item::Impl(id) => served_impl(db, id).is_some(),
        Item::Trait(id) => served_trait(db, id).is_some(),
        Item::Function(id) => served_function(db, id).is_some(),
        Item::TypeAlias(id) => served_type_alias(db, id).is_some(),
        Item::Fields(id) => served_variant_fields(db, id).is_some(),
        Item::Variants(id) => served_enum_variants(db, id).is_some(),
    }
}

#[test]
fn serving_a_signature_parses_nothing() {
    let _turn = turn();
    let memory = stored(FIXTURE);
    let db = served_db(FIXTURE, &memory, Mode::Serve);
    let krate = crate_named(&db, "lib");
    for krate in all(&db) {
        _ = crate_def_map(&db, krate);
    }
    let items = declared(&db, krate);
    // The keys' module indexes, made once per crate from its def map.
    assert!(serve_one(&db, items[0]));
    let executed = db.executed_in_read_order(|| {
        for &item in &items {
            assert!(serve_one(&db, item), "{item:?}");
        }
    });
    let parsed: Vec<&String> = (executed.iter())
        .filter(|it| {
            ["parse", "ast_id_map", "item_tree", "signature_syntax", "with_source_map", "relative"]
                .iter()
                .any(|query| it.contains(query))
        })
        .collect();
    assert_eq!(parsed, Vec::<&String>::new(), "{executed:?}");
}

#[test]
fn finding_a_record_allocates_nothing() {
    let _turn = turn();
    let memory = stored(FIXTURE);
    let db = served_db(FIXTURE, &memory, Mode::Serve);
    let krate = crate_named(&db, "lib");
    let images = DepImages::try_get(&db).unwrap().images(&db);
    let part = images[&krate].view().part(Part::Signatures);
    let items = declared(&db, krate);
    // The keys' module indexes, made once per crate from its def map.
    find(&db, part, items[0]).unwrap();
    blaze_alloc_count::assert_at_most(0, "finding the record of every item", || {
        for &item in &items {
            std::hint::black_box(find(&db, part, item).unwrap());
        }
    });
}

#[test]
fn a_memo_hit_of_a_served_signature_allocates_nothing() {
    let _turn = turn();
    let memory = stored(FIXTURE);
    let db = served_db(FIXTURE, &memory, Mode::Serve);
    let items = declared(&db, crate_named(&db, "lib"));
    let functions: Vec<FunctionId> = (items.iter())
        .filter_map(|it| match *it {
            Item::Function(it) => Some(it),
            _ => None,
        })
        .collect();
    functions.iter().for_each(|&it| _ = FunctionSignature::of(&db, it));
    blaze_alloc_count::assert_at_most(0, "a served signature asked again", || {
        functions.iter().for_each(|&it| _ = std::hint::black_box(FunctionSignature::of(&db, it)));
    });
}

/// `item`'s record of `of`, which reads back as `of` in the database that wrote it.
fn round_trip<S: Sig>(db: &TestDB, krate: Crate, item: Item, of: &S::Of) -> std::sync::Arc<Aligned>
where
    S::Of: PartialEq + std::fmt::Debug,
{
    let record = record::<S>(db, krate, item.what(), of).unwrap();
    let bytes = std::sync::Arc::new(Aligned::copy_from(&record));
    let held: crate::deps::ImageBuf = bytes.clone();
    let value = read::<S>(db, krate, item.what(), &held, &bytes).unwrap();
    assert_eq!(S::of(&value), of, "{item:?}");
    bytes
}

#[test]
fn a_relocated_store_reads_back_as_it_was() {
    let db = fixture::db(FIXTURE).into_db();
    let krate = crate_named(&db, "lib");
    let mut relocated = 0;
    for item in items(&db, krate) {
        let (bytes, store) = match item {
            Item::Struct(id) => {
                let of = StructSignature::of(&db, id);
                (round_trip::<codes::Struct>(&db, krate, item, of), Some(&of.store))
            }
            Item::Function(id) => {
                let of = FunctionSignature::of(&db, id);
                (round_trip::<codes::Function>(&db, krate, item, of), Some(&of.store))
            }
            Item::Impl(id) => {
                let of = ImplSignature::of(&db, id);
                (round_trip::<codes::Impl>(&db, krate, item, of), Some(&of.store))
            }
            Item::Fields(id) => {
                let of = VariantFields::of(&db, id);
                (round_trip::<codes::Fields>(&db, krate, item, of), Some(&of.store))
            }
            Item::Variants(id) => {
                let of = EnumVariants::of(&db, id);
                (round_trip::<codes::Variants>(&db, krate, item, of), None)
            }
            _ => continue,
        };
        let Some(store) = store else { continue };
        let mut table = Relocation::new();
        _ = relocating(&mut table, || blaze_format::to_bytes(store));
        if table.is_empty() {
            continue;
        }
        relocated += 1;
        let rec = open_as::<RecordRoot>(&bytes, RECORD_SCHEMA).unwrap();
        let written = rec.store.follow(&bytes).unwrap();
        assert_ne!(written, blaze_format::to_bytes(store), "{item:?} names its ids by index");
    }
    assert!(relocated > 0, "some store names a crate, a file or a syntax context");
}

/// A store written outside a relocation is today's bytes, read back as today, and one that
/// names no id is the same bytes inside one.
#[test]
fn a_store_outside_a_relocation_is_written_as_before() {
    let db = fixture::db(FIXTURE).into_db();
    let krate = crate_named(&db, "lib");
    for item in items(&db, krate) {
        let (store, source_map) = match item {
            Item::Struct(id) => {
                let (it, map) = StructSignature::with_source_map(&db, id);
                (&it.store, map)
            }
            Item::Function(id) => {
                let (it, map) = FunctionSignature::with_source_map(&db, id);
                (&it.store, map)
            }
            Item::TypeAlias(id) => {
                let (it, map) = TypeAliasSignature::with_source_map(&db, id);
                (&it.store, map)
            }
            _ => continue,
        };
        crate::expr_store::persist::assert_store_round_trip(store, source_map);
        let mut table = Relocation::new();
        let inside = relocating(&mut table, || blaze_format::to_bytes(store));
        if table.is_empty() {
            assert_eq!(inside, blaze_format::to_bytes(store), "{item:?}");
        }
    }
}

/// The function `f` of `lib` in `db`.
/// Signatures whose stores keep macro calls: a type macro and macros in array lengths.
const CALLS: &str = r#"
//- /lib.rs crate:lib new_source_root:library
macro_rules! m { () => { u8 } }
macro_rules! n { () => { 4 } }
pub fn f(_: [u8; n!()]) -> m!() { 0 }
pub struct S([m!(); n!()]);
"#;

/// A store's macro calls are moved through the record's table: served in another database,
/// they are that database's calls, the ones lowering it makes, not the writer's ids.
#[test]
fn a_store_s_macro_calls_read_back_as_this_database_s() {
    let _turn = turn();
    let memory = stored(CALLS);
    let db = served_db(CALLS, &memory, Mode::Serve);
    let krate = crate_named(&db, "lib");
    let f = function_f(&db);
    let served = served_function(&db, f).expect("an item of a crate with an image");
    let lowered = &FunctionSignature::with_source_map(&db, f).0;
    assert!(!lowered.store.macro_calls().is_empty(), "the fixture's calls are in the store");
    assert_eq!(served.store.macro_calls(), lowered.store.macro_calls());
    for item in declared(&db, krate) {
        assert_served(&db, krate, item, true);
    }
}

fn function_f(db: &TestDB) -> FunctionId {
    let f = Name::new_symbol_root(intern::Symbol::intern("f"));
    let krate = crate_named(db, "lib");
    (declared(db, krate).into_iter())
        .find_map(|item| match item {
            Item::Function(it) if FunctionSignature::with_source_map(db, it).0.name == f => {
                Some(it)
            }
            _ => None,
        })
        .unwrap()
}

/// `memory` with the image of `lib` in a database of `fixture`, its signatures part replaced
/// by `part`.
fn with_signatures(fixture: &str, memory: &Memory, part: &[u8]) {
    let db = fixture::db(fixture).into_db();
    let lib = crate_named(&db, "lib");
    let key = keys(&db, &[lib], BUILD)[&lib];
    let image = memory.images.lock()[&key].clone();
    let image = ImageRef::open(&image, key).unwrap();
    let bytes = write_image(key, [image.part(Part::DefMap), image.part(Part::LangItems), part]);
    memory.images.lock().insert(key, std::sync::Arc::new(Aligned::copy_from(&bytes)));
}

/// The signatures part of `lib` in a fresh database of `fixture`.
fn part_of(fixture: &str) -> Vec<u8> {
    let db = fixture::db(fixture).into_db();
    encode(&db, crate_named(&db, "lib"), &mut Vec::new()).unwrap()
}

#[test]
fn checking_finds_a_difference_and_serves_the_image() {
    let _turn = turn();
    let other = FIXTURE.replace("pub fn f() -> u8 { 0 }", "pub fn f() -> u16 { 0 }");
    let memory = stored(&other);
    with_signatures(&other, &memory, &part_of(FIXTURE));
    _ = frozen::differences();
    for mode in [Mode::Serve, Mode::Check] {
        let db = served_db(&other, &memory, mode);
        let f = function_f(&db);
        let served = served_function(&db, f).unwrap();
        let lowered = &FunctionSignature::with_source_map(&db, f).0;
        assert_ne!(*served, **lowered, "the image's `f` returns a u8, the file's a u16");
        let differences = frozen::differences();
        match mode {
            Mode::Check => assert!(
                differences.iter().any(|it| it.contains("signatures") && it.contains("Function")),
                "{differences:?}"
            ),
            _ => assert_eq!(differences, Vec::<String>::new()),
        }
    }
}

#[test]
#[should_panic(expected = "has no record of the item")]
fn an_image_without_the_item_is_corrupt() {
    let memory = stored(FIXTURE);
    let mut out = Writer::new(SCHEMA);
    let at = out.next::<PartRoot>();
    out.push(&PartRoot { keys: Slice::EMPTY, places: Slice::EMPTY });
    with_signatures(FIXTURE, &memory, &out.finish(at));
    let db = served_db(FIXTURE, &memory, Mode::Serve);
    served_function(&db, function_f(&db));
}

#[test]
#[should_panic(expected = "is corrupt")]
fn a_signatures_part_that_does_not_open_is_corrupt() {
    let memory = stored(FIXTURE);
    with_signatures(FIXTURE, &memory, &[0; 64]);
    let db = served_db(FIXTURE, &memory, Mode::Serve);
    served_function(&db, function_f(&db));
}

#[test]
fn a_record_of_another_query_is_refused() {
    let db = fixture::db(FIXTURE).into_db();
    let krate = crate_named(&db, "lib");
    let f = function_f(&db);
    let what = Item::Function(f).what();
    let written = record::<codes::Function>(&db, krate, what, FunctionSignature::of(&db, f));
    let bytes = std::sync::Arc::new(Aligned::copy_from(&written.unwrap()));
    let image: crate::deps::ImageBuf = bytes.clone();
    assert!(read::<codes::Function>(&db, krate, what, &image, &bytes).is_ok());
    let a_struct = items(&db, krate).into_iter().find(|it| matches!(it, Item::Struct(_)));
    let error = read::<codes::Struct>(&db, krate, a_struct.unwrap().what(), &image, &bytes);
    assert!(error.is_err_and(|it| it.contains("a record of query")));
}

#[test]
fn a_key_names_one_item_and_is_the_same_in_every_database() {
    let keys_of = |db: &TestDB| -> (HashSet<ContentId>, usize) {
        let items = items(db, crate_named(db, "lib"));
        let distinct: HashSet<Item> = items.iter().copied().collect();
        let keys = items.iter().map(|&item| key::of(db, item).unwrap().1).collect();
        (keys, distinct.len())
    };
    let (keys, items) = keys_of(&fixture::db(FIXTURE).into_db());
    assert_eq!(keys.len(), items, "one key per item");
    assert_eq!(keys_of(&fixture::db(FIXTURE).into_db()).0, keys);
}
