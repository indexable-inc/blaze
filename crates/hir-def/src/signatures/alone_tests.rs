//! Each `of` lowers without a source map and keeps the value in its memo: the same value as
//! `with_source_map`'s and as the item's file lowered given `of`'s blocks, and no read of it
//! runs a query that builds a map.

use hir_expand::fixture;

use super::*;
use crate::{
    AssocItemId, ModuleDefId,
    nameres::assoc::{ImplItems, TraitItems},
    test_db::{TestDB, TestDBExt},
    test_items::{declarations, impls},
};

/// The fixtures of hir-def's signature tests: `relative_tests`', `expr_store/tests/signatures.rs`'
/// and `deps/signatures/tests.rs`' (every kind of signature across crates, items a macro writes,
/// a `#[path]` module and an `include!`d file; const arguments that are blocks with items).
const FIXTURES: &[&str] = &[
    super::relative_tests::CODE,
    r#"
pub fn outer() -> u32 { struct Inner<T>(T); 1 }
pub fn p(x: [u8; { struct InType; 1 }]) -> u8 { x[0] }
"#,
    r"
struct S { field: foo, }
struct S(i32, u32, &'static str);
#[repr(Rust)]
struct S;

struct S<'a, 'b, T: Clone, const C: usize = 3, X = ()> where X: Default, for<'a, 'c> fn() -> i32: for<'b> Trait<'a, Item = Boo>;
#[repr(C, packed)]
struct S {}
",
    r#"
fn foo<'a, const C: usize = 314235, T: Trait<Item = A> = B>(Struct { foo: bar }: &Struct, _: (), a: u32) -> &'a dyn Fn() -> i32 where (): Default {}
const async unsafe extern "C" fn a() {}
fn ret_impl_trait() -> impl Trait {}
"#,
    r"
fn f<T = impl Trait>() {}
",
    r"
fn impl_trait_args<T>(_: impl Trait) {}
fn impl_trait_args2<T>(_: impl Trait<impl Trait>) {}

fn impl_trait_ret<T>() -> impl Trait {}
fn impl_trait_ret2<T>() -> impl Trait<impl Trait> {}

fn not_allowed1(f: impl Fn(impl Foo)) {
    let foo = S;
    f(foo);
}

fn not_allowed2(f: impl Fn(&impl Foo)) {
    let foo = S;
    f(&foo);
}

fn not_allowed3(bar: impl Bar<impl Foo>) {}

fn not_allowed4(bar: impl Bar<&impl Foo>) {}

fn allowed1(baz: impl Baz<Assoc = impl Foo>) {}

fn allowed2<'a>(baz: impl Baz<Assoc = &'a (impl Foo + 'a)>) {}

fn allowed3(baz: impl Baz<Assoc = Qux<impl Foo>>) {}
",
    r#"
type Alias<'a, 'b, T> = &'b T;
fn f<T>(_: Alias<T>) {}
"#,
    r#"
fn foo(v: for<'a> Trait1 + Trait2) {}
"#,
    r#"
extern "C" {
    fn extern_fn();
}
"#,
    r#"
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
"#,
    r#"
//- /lib.rs crate:lib new_source_root:library
pub struct Arr<const N: usize>([u8; N]);
pub struct Blocky(pub [u8; { const A: usize = 1; A + 1 }]);
pub fn nested() -> Arr<{ const A: usize = 1; { const B: usize = A; B } }> { loop {} }
pub type Alias = [u8; { struct Q; 2 }];
"#,
];

fn assoc(item: AssocItemId) -> GenericDefId {
    match item {
        AssocItemId::FunctionId(it) => it.into(),
        AssocItemId::ConstId(it) => it.into(),
        AssocItemId::TypeAliasId(it) => it.into(),
    }
}

/// Every signature of every crate of `db`, items of traits and impls too, and every variant.
fn defs(db: &TestDB) -> (Vec<GenericDefId>, Vec<VariantId>) {
    let (mut signatures, mut variants) = (Vec::new(), Vec::new());
    let crates: Vec<_> = base_db::all_crates(db).iter().copied().collect();
    for krate in crates {
        for decl in declarations(db, krate) {
            match decl {
                ModuleDefId::FunctionId(it) => signatures.push(it.into()),
                ModuleDefId::ConstId(it) => signatures.push(it.into()),
                ModuleDefId::StaticId(it) => signatures.push(it.into()),
                ModuleDefId::TypeAliasId(it) => signatures.push(it.into()),
                ModuleDefId::TraitId(it) => {
                    signatures.push(it.into());
                    let items = TraitItems::query(db, it).items.iter();
                    signatures.extend(items.map(|&(_, item)| assoc(item)));
                }
                ModuleDefId::AdtId(it) => {
                    signatures.push(it.into());
                    match it {
                        AdtId::StructId(it) => variants.push(it.into()),
                        AdtId::UnionId(it) => variants.push(it.into()),
                        AdtId::EnumId(it) => {
                            let of = EnumVariants::of(db, it).0.variants.values();
                            variants.extend(of.map(|&(v, _)| VariantId::from(v)));
                        }
                    }
                }
                _ => {}
            }
        }
        for imp in impls(db, krate) {
            signatures.push(imp.into());
            let items = ImplItems::of(db, imp).0.items.iter();
            signatures.extend(items.map(|&(_, item)| assoc(item)));
        }
    }
    (signatures, variants)
}

/// Reads `def`'s signature through `of` only.
fn read_of(db: &TestDB, def: GenericDefId) {
    use std::hint::black_box;
    match def {
        GenericDefId::AdtId(AdtId::StructId(it)) => _ = black_box(StructSignature::of(db, it)),
        GenericDefId::AdtId(AdtId::UnionId(it)) => _ = black_box(UnionSignature::of(db, it)),
        GenericDefId::AdtId(AdtId::EnumId(it)) => _ = black_box(EnumSignature::of(db, it)),
        GenericDefId::ConstId(it) => _ = black_box(ConstSignature::of(db, it)),
        GenericDefId::StaticId(it) => _ = black_box(StaticSignature::of(db, it)),
        GenericDefId::ImplId(it) => _ = black_box(ImplSignature::of(db, it)),
        GenericDefId::TraitId(it) => _ = black_box(TraitSignature::of(db, it)),
        GenericDefId::FunctionId(it) => _ = black_box(FunctionSignature::of(db, it)),
        GenericDefId::TypeAliasId(it) => _ = black_box(TypeAliasSignature::of(db, it)),
    }
}

#[test]
fn of_is_the_lowering_with_the_source_map() {
    macro_rules! same {
        ($db:ident, $Sig:ident, $id:expr) => {{
            let id = $id;
            let of = $Sig::of(&$db, id);
            let (with_map, map) = $Sig::with_source_map(&$db, id);
            assert_eq!(*of, **with_map, "{id:?}: `of` and `with_source_map`");
            let source = id.lookup(&$db).source_view(&$db);
            let lowering = BodyLowering::SourceMap { blocks: of.store.block_scopes() };
            let (file, file_map) = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |_| {
                $Sig::lower(&$db, id, source, None, lowering)
            });
            assert_eq!(*of, file, "{id:?}: `of` and its file's lowering");
            assert!(*map == file_map, "{id:?}: {map:#?}\nfrom the file: {file_map:#?}");
        }};
    }
    for fixture in FIXTURES {
        let db = fixture::db(fixture).into_db();
        let (signatures, variants) = defs(&db);
        assert!(!signatures.is_empty(), "no signature in {fixture}");
        for def in signatures {
            match def {
                GenericDefId::AdtId(AdtId::StructId(it)) => same!(db, StructSignature, it),
                GenericDefId::AdtId(AdtId::UnionId(it)) => same!(db, UnionSignature, it),
                GenericDefId::AdtId(AdtId::EnumId(it)) => same!(db, EnumSignature, it),
                GenericDefId::ConstId(it) => same!(db, ConstSignature, it),
                GenericDefId::StaticId(it) => same!(db, StaticSignature, it),
                GenericDefId::ImplId(it) => same!(db, ImplSignature, it),
                GenericDefId::TraitId(it) => same!(db, TraitSignature, it),
                GenericDefId::FunctionId(it) => same!(db, FunctionSignature, it),
                GenericDefId::TypeAliasId(it) => same!(db, TypeAliasSignature, it),
            }
        }
        for id in variants {
            let of = VariantFields::of(&db, id);
            let (with_map, map) = VariantFields::with_source_map(&db, id);
            assert_eq!(*of, **with_map, "{id:?}: `of` and `with_source_map`");
            let lowering = BodyLowering::SourceMap { blocks: of.store.block_scopes() };
            let (file, file_map) = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
                VariantFields::lower(&db, id, None, lowering, frame)
            });
            assert_eq!(*of, file, "{id:?}: `of` and its file's lowering");
            assert!(*map == file_map, "{id:?}: {map:#?}\nfrom the file: {file_map:#?}");
        }
    }
}

/// A read of every signature and every variant's fields through `of` runs no query that
/// builds a source map.
#[test]
fn of_builds_no_source_map() {
    for fixture in FIXTURES {
        let db = fixture::db(fixture).into_db();
        let (signatures, variants) = defs(&db);
        let log = db.executed_in_read_order(|| {
            for &def in &signatures {
                read_of(&db, def);
            }
            for &id in &variants {
                _ = std::hint::black_box(VariantFields::of(&db, id));
            }
        });
        let builds_a_map = |it: &&String| it.contains("::relative") || it.contains("::with_source_map");
        let built: Vec<_> = log.iter().filter(builds_a_map).collect();
        assert!(built.is_empty(), "{fixture}\n{built:#?}");
        assert!(log.iter().any(|it| it.contains("::kept")), "{fixture}\n{log:#?}");
    }
}

/// `#[rustc_deny_explicit_impl]`, written directly or through `cfg_attr`, sets
/// `TraitFlags::DENY_EXPLICIT_IMPL`; a path to it or an argument does not.
#[test]
fn deny_explicit_impl_is_a_trait_flag() {
    let db = fixture::db(
        r#"
#[rustc_deny_explicit_impl] pub trait D {}
#[cfg_attr(all(), rustc_deny_explicit_impl)] pub trait C {}
#[cfg_attr(any(), rustc_deny_explicit_impl)] pub trait Off {}
#[a::rustc_deny_explicit_impl] pub trait Q {}
#[rustc_deny_explicit_impl(x)] pub trait Arg {}
pub trait Plain {}
"#,
    )
    .into_db();
    let mut seen = Vec::new();
    for decl in declarations(&db, db.fetch_test_crate()) {
        let ModuleDefId::TraitId(it) = decl else { continue };
        let signature = TraitSignature::of(&db, it);
        let denied = signature.flags.contains(TraitFlags::DENY_EXPLICIT_IMPL);
        let name = signature.name.as_str().to_owned();
        assert_eq!(denied, matches!(name.as_str(), "D" | "C"), "{name}");
        seen.push(name);
    }
    seen.sort_unstable();
    assert_eq!(seen, ["Arg", "C", "D", "Off", "Plain", "Q"]);
}
