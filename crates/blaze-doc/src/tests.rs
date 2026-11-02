//! The model and pages of a fixture workspace with every item kind, re-exports, intra-doc links
//! (one unresolved), a dependency and a proc macro.

use base_db::Db;
use hir::Crate;
use hir_expand::fixture;

use crate::{
    Asked,
    model::{CrateDoc, Item, Kind, Model, Target},
};

const FIXTURE: &str = r#"
//- proc_macros: identity, derive_identity, mirror
//- minicore: sized, copy, clone, derive, option, ord, iterator, send, sync, unpin, non_null, cell, phantom_data, deref, deref_mut, slice, from
//- /lib.rs crate:fixture@0.3.1,https://example.com/fixture.git deps:dep
//! The fixture crate. See [`Thing`], [`shapes::Square`] and [`missing_item`].
//!
//! # Usage
//!
//! ```
//! let thing = fixture::Thing::new(1);
//! # let hidden = 2;
//! ```

pub mod shapes;

pub use dep::Helper;
pub use dep::nested::*;
pub use proc_macros::DeriveIdentity;
#[doc(no_inline)]
pub use dep::Outside;

mod private {
    /// Reachable only through a re-export.
    pub struct Inner;
}
pub use private::Inner as Shown;
pub use shapes::Corner;

#[doc(hidden)]
pub struct Secret;

/// A thing. Make one with [Self::new]; see [`Kind::A`], [`free()`], [dep::Helper], [dep::Deep],
/// [`Option`] and [the shape trait](Shape).
#[derive(Clone)]
pub struct Thing<T: Clone, const N: usize> {
    /// The value.
    pub value: T,
    hidden: [u8; N],
}

impl<T: Clone, const N: usize> Thing<T, N> {
    /// Makes one.
    pub fn new(value: T) -> Self {
        loop {}
    }

    pub(crate) fn private_helper(&self) {}
}

/// Kinds of things.
#[derive(Clone)]
pub enum Kind {
    /// The first.
    A,
    B(u32),
    C {
        /// A field of a variant.
        x: i32,
    },
}

/// Either one.
pub union Bits {
    pub int: u32,
    pub float: f32,
}

/// A shape.
pub trait Shape: Clone {
    /// What it makes.
    type Out;
    const SIDES: usize;
    /// Its area.
    fn area(&self) -> u32;
    fn name(&self) -> &'static str {
        "shape"
    }
}

impl Shape for Kind {
    type Out = u8;
    const SIDES: usize = 3;
    fn area(&self) -> u32 {
        0
    }
}

/// Free. Unresolved: [Nope].
pub fn free<'a, I>(input: &'a str, items: I) -> impl Iterator<Item = u8> + 'a
where
    I: Iterator<Item = u32> + Clone,
{
    loop {}
}

pub unsafe extern "C" fn ffi(ptr: *const u8, len: usize) -> Option<&'static [u8]> {
    loop {}
}

/// An alias.
pub type Alias<T> = Option<T>;

/// An alias with its argument.
pub type Maybe = Option<shapes::Square>;

/// The most.
pub const MAX: u32 = 10;

pub static mut COUNT: u32 = 0;

/// Holds a raw pointer, so is neither `Send` nor `Sync`.
pub struct Raw(*const u8);

/// Derefs to a `Chain`, and through it to bytes.
pub struct Bytes([u8; 4]);
impl core::ops::Deref for Bytes {
    type Target = Chain;
    fn deref(&self) -> &Chain {
        loop {}
    }
}

/// A link in a chain of derefs.
pub struct Chain;
impl Chain {
    /// Reads.
    pub fn read(&self) {}
    /// Writes, which needs `DerefMut`.
    pub fn write(&mut self) {}
    /// Consumes, which needs `Chain: Copy`.
    pub fn take(self) {}
    /// Makes one, with no receiver.
    pub fn new() -> Chain {
        Chain
    }
}
impl core::ops::Deref for Chain {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        loop {}
    }
}

pub struct NotSend;
impl !Send for NotSend {}
/// Neither `Send`, from its field, nor `Sync`, from its parameter's.
pub struct Wraps<T>(NotSend, T);

/// Makes a thing.
#[macro_export]
macro_rules! make {
    ($value:expr) => { $value };
    () => {};
}

dep::declare! {
    /// Declared by a macro of another crate, where rustc reports no lint: [Foreign].
    Declared
}
macro_rules! declare_here {
    ($(#[doc = $doc:literal])* $name:ident) => { $(#[doc = $doc])* pub struct $name; };
}
declare_here! {
    /// Declared by a macro of this crate: [Homegrown].
    DeclaredHere
}

#[deprecated(since = "1.0", note = "use `free`")]
pub fn old() {}

#[must_use]
pub fn important() -> u32 {
    0
}

/// Yields what it holds.
pub trait Each {
    type Item;
    /// Only where the items are `Copy`.
    fn copied(self)
    where
        Self::Item: Copy,
        Self: Sized,
    {
    }
    /// A bound of a trait the solver looks up impls of, after one that holds, as
    /// `Iterator::max` has it.
    fn least(self)
    where
        Self: Sized,
        Self::Item: Ord,
    {
    }
    /// A higher-ranked bound, which every item meets.
    fn borrowed(&self)
    where
        for<'a> &'a Self::Item: Copy,
    {
    }
}
/// Yields squares, which are not `Copy`.
pub struct Squares;
impl Each for Squares {
    type Item = shapes::Square;
}
/// Yields squares too, over a parameter with a bound of the impl's own.
pub struct Finds<A>(A);
impl<A: Clone> Each for Finds<A> {
    type Item = shapes::Square;
}
/// Yields squares from what it borrows, an impl of lifetimes alone.
pub struct Borrows<'a>(&'a u8);
impl<'a> Each for Borrows<'a> {
    type Item = shapes::Square;
}
/// Yields helpers, not `Copy` either, though their crate could make them so.
pub struct Helpers;
impl Each for Helpers {
    type Item = dep::Helper;
}

//- /shapes.rs
/// A square.
pub struct Square;

impl Square {
    /// Its side. Unresolved, and `cfg(doc)` holds: [Loud].
    #[cfg_attr(not(doc), allow(rustdoc::broken_intra_doc_links))]
    pub const SIDE: u32 = 1;
}

mod corner {
    #![allow(rustdoc::broken_intra_doc_links)]
    /// Documented in `shapes`, which re-exports it publicly. Unresolved, allowed: [Quiet].
    pub struct Corner;
}
pub use corner::Corner;

//- /dep.rs crate:dep@1.2.0,https://example.com/dep.git
#![doc(html_root_url = "https://docs.rs/dep/1.2.0")]
/// Helps.
pub struct Helper;

pub struct Outside;

pub mod nested {
    /// Found through a glob.
    pub fn glob_fn() {}
}

mod deep {
    /// Declared in a private module, documented at the root.
    pub struct Deep;
}
pub use deep::Deep;

/// Declares a unit struct with the docs given.
#[macro_export]
macro_rules! declare {
    ($(#[doc = $doc:literal])* $name:ident) => { $(#[doc = $doc])* pub struct $name; };
}
"#;

fn load() -> (Db, Model) {
    load_with(false, false)
}

/// The fixture documented; `dep_for_tests` makes `dep` a dependency of its tests alone.
fn load_with(no_deps: bool, dep_for_tests: bool) -> (Db, Model) {
    load_text(FIXTURE, no_deps, dep_for_tests)
}

/// `text` documented: a fixture with crates `fixture` and `dep`.
fn load_text(text: &str, no_deps: bool, dep_for_tests: bool) -> (Db, Model) {
    let mut db = fixture::db(text).into_db();
    let named = |name: &str| {
        Crate::all(&db).into_iter().find(|it| crate::crate_name(&db, *it) == name).expect(name)
    };
    let (fixture, dep) = (named("fixture"), named("dep"));
    let mut test_only = crate::TestOnly::default();
    if dep_for_tests {
        test_only.insert((fixture, dep));
    }
    let asked = [Asked { krate: fixture, private: false }];
    let crates = crate::crates_to_document(&db, &asked, no_deps, &test_only);
    crate::set_cfg_doc(&mut db, &crates);
    let model = crate::model(&db, crates, test_only);
    (db, model)
}

fn krate<'a>(model: &'a Model, name: &str) -> &'a CrateDoc {
    model.krate(name).unwrap_or_else(|| panic!("crate {name} documented"))
}

/// The item at `path`, written from the crate's name.
fn item<'a>(krate: &'a CrateDoc, path: &str) -> &'a Item {
    let inner = path.strip_prefix(krate.name.as_str()).unwrap_or(path);
    let inner = inner.strip_prefix("::").unwrap_or(inner);
    krate.find(inner).unwrap_or_else(|| {
        let all: Vec<_> = krate.items.iter().map(|it| it.full_path()).collect();
        panic!("{path} documented; the crate has {all:?}")
    })
}

fn link<'a>(item: &'a Item, text: &str) -> &'a Target {
    item.docs
        .links
        .get(text)
        .unwrap_or_else(|| panic!("link {text} resolved; resolved are {:?}", item.docs.links))
}

fn page(target: &Target) -> String {
    match target {
        Target::Item(it) => match &it.anchor {
            Some(anchor) => format!("{}#{anchor}", it.page()),
            None => it.page(),
        },
        Target::Url(url) => url.clone(),
    }
}

#[test]
fn documents_the_crate_and_its_dependencies() {
    let (_db, model) = load();
    let names: Vec<_> = model.crates.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names[0], "fixture");
    assert!(names.contains(&"dep"), "{names:?}");
    assert!(names.contains(&"proc_macros"), "{names:?}");
    assert!(!names.contains(&"core"), "{names:?}");
    assert_eq!(krate(&model, "fixture").version.as_deref(), Some("0.3.1"));
}

#[test]
fn every_item_kind_has_its_page() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    for (path, kind) in [
        ("fixture", Kind::Module),
        ("fixture::shapes", Kind::Module),
        ("fixture::shapes::Square", Kind::Struct),
        ("fixture::Thing", Kind::Struct),
        ("fixture::Kind", Kind::Enum),
        ("fixture::Bits", Kind::Union),
        ("fixture::Shape", Kind::Trait),
        ("fixture::free", Kind::Function),
        ("fixture::ffi", Kind::Function),
        ("fixture::Alias", Kind::TypeAlias),
        ("fixture::MAX", Kind::Constant),
        ("fixture::COUNT", Kind::Static),
        ("fixture::make", Kind::Macro),
        ("fixture::old", Kind::Function),
        ("fixture::important", Kind::Function),
    ] {
        assert_eq!(item(fixture, path).kind, kind, "{path}");
    }
    assert!(fixture.find("Secret").is_none(), "doc(hidden) items have no page");
}

#[test]
fn reexports_inline_as_rustdoc_does() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    assert_eq!(item(fixture, "fixture::Helper").kind, Kind::Struct, "a foreign item is inlined");
    assert_eq!(item(fixture, "fixture::glob_fn").kind, Kind::Function, "a foreign glob is inlined");
    assert_eq!(item(fixture, "fixture::DeriveIdentity").kind, Kind::ProcDerive);
    assert_eq!(
        item(fixture, "fixture::Shown").kind,
        Kind::Struct,
        "an unreachable item is inlined"
    );
    assert!(fixture.find("Outside").is_none(), "doc(no_inline) stays a line");
    let root = fixture.root();
    let lines: Vec<String> = root.reexports.iter().map(|it| it.decl.plain()).collect();
    assert!(lines.iter().any(|it| it == "pub use dep::Outside;"), "{lines:?}");
    assert!(item(fixture, "fixture::Helper").docs.markdown.contains("Helps."));
    // `shapes` re-exports it publicly, so the root's `use` of that re-export stays a line.
    assert_eq!(item(fixture, "fixture::shapes::Corner").kind, Kind::Struct);
    assert!(fixture.find("Corner").is_none(), "re-exported through a public re-export: {lines:?}");
    assert!(lines.iter().any(|it| it == "pub use fixture::shapes::Corner;"), "{lines:?}");
}

#[test]
fn aliases_show_the_type_they_stand_for() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    let alias = item(fixture, "fixture::Alias");
    let aliased = alias.aliased.as_ref().expect("an alias of an enum shows it").plain();
    assert!(aliased.starts_with("pub enum Alias<T> {"), "{aliased}");
    let variants: Vec<String> = alias.variants.iter().map(|it| it.decl.plain()).collect();
    assert_eq!(variants, ["None", "Some(T)"]);
    let maybe = item(fixture, "fixture::Maybe");
    let aliased = maybe.aliased.as_ref().expect("an alias of an enum shows it").plain();
    assert!(aliased.contains("Some(Square)"), "the alias's argument for the parameter: {aliased}");
    let variants: Vec<String> = maybe.variants.iter().map(|it| it.decl.plain()).collect();
    assert_eq!(variants, ["None", "Some(Square)"]);
}

#[test]
fn unsafe_binders_show_their_lifetimes_as_rustdoc_does() {
    let (_db, model) = load_text(
        r#"
//- /lib.rs crate:fixture deps:dep
pub type Bound = unsafe<'a, 'b> &'a &'b u8;
pub type Unbound = unsafe<> u8;
//- /dep.rs crate:dep
"#,
        false,
        false,
    );
    let fixture = krate(&model, "fixture");
    assert_eq!(
        item(fixture, "fixture::Bound").decl.plain(),
        "pub type Bound = unsafe<'a, 'b> &'a &'b u8;"
    );
    assert_eq!(
        item(fixture, "fixture::Unbound").decl.plain(),
        "pub type Unbound = u8;",
        "no lifetimes, no `unsafe<>`"
    );
}

#[test]
fn signatures_come_from_the_hir() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    assert_eq!(
        item(fixture, "fixture::free").decl.plain(),
        "pub fn free<'a, I>(input: &'a str, items: I) -> impl Iterator<Item = u8> + 'a\nwhere\n    I: Iterator<Item = u32> + Clone,"
    );
    assert_eq!(
        item(fixture, "fixture::ffi").decl.plain(),
        "pub unsafe extern \"C\" fn ffi(ptr: *const u8, len: usize) -> Option<&'static [u8]>"
    );
    let thing = item(fixture, "fixture::Thing").decl.plain();
    assert!(thing.starts_with("pub struct Thing<T: Clone, const N: usize> {"), "{thing}");
    assert!(thing.contains("pub value: T,"), "{thing}");
    assert!(thing.contains("/* private fields */"), "{thing}");
    assert_eq!(item(fixture, "fixture::Alias").decl.plain(), "pub type Alias<T> = Option<T>;");
    assert_eq!(item(fixture, "fixture::MAX").decl.plain(), "pub const MAX: u32 = 10;");
    assert_eq!(item(fixture, "fixture::COUNT").decl.plain(), "pub static mut COUNT: u32");
    let make = item(fixture, "fixture::make").decl.plain();
    assert!(make.starts_with("macro_rules! make {"), "{make}");
    assert!(make.contains("($value:expr) => { ... };"), "{make}");
    let shape = item(fixture, "fixture::Shape").decl.plain();
    assert!(shape.starts_with("pub trait Shape: Clone {"), "{shape}");
    assert!(shape.contains("fn area(&self) -> u32;"), "{shape}");
    assert!(shape.contains("fn name(&self) -> &'static str { ... }"), "{shape}");
    for group in [
        "// Required associated type",
        "// Associated constant",
        "// Required method",
        "// Provided method",
    ] {
        assert!(shape.contains(&format!("{group}\n")), "{group}: {shape}");
    }
    assert!(!shape.contains("// Provided associated type"), "{shape}");
}

#[test]
fn signature_names_link_to_their_pages() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    let ffi = item(fixture, "fixture::ffi");
    let targets: Vec<String> = ffi.decl.links().map(|(_, to)| page(to)).collect();
    assert!(targets.iter().any(|it| it == "core/option/enum.Option.html"), "{targets:?}");
    assert!(targets.iter().any(|it| it == "std/primitive.u8.html"), "{targets:?}");
}

#[test]
fn members_and_impls() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    let thing = item(fixture, "fixture::Thing");
    assert_eq!(thing.fields.iter().map(|it| it.name.as_str()).collect::<Vec<_>>(), ["value"]);
    assert!(thing.hidden_fields);
    let inherent: Vec<&str> = thing
        .impls
        .inherent
        .iter()
        .flat_map(|it| it.items.iter().map(|it| it.name.as_str()))
        .collect();
    assert_eq!(inherent, ["new"], "a pub(crate) method is not shown");
    let traits: Vec<String> = thing.impls.traits.iter().map(|it| it.header.plain()).collect();
    assert!(traits.iter().any(|it| it.contains("Clone for Thing<T, N>")), "{traits:?}");
    let auto: Vec<String> = thing.impls.auto.iter().map(|it| it.header.plain()).collect();
    assert!(
        auto.contains(
            &"impl<T, const N: usize> Send for Thing<T, N>\nwhere\n    T: Send,".to_owned()
        ),
        "{auto:?}"
    );
    assert!(
        auto.contains(
            &"impl<T, const N: usize> Sync for Thing<T, N>\nwhere\n    T: Sync,".to_owned()
        ),
        "{auto:?}"
    );
    let blanket: Vec<String> = thing.impls.blanket.iter().map(|it| it.header.plain()).collect();
    assert!(blanket.contains(&"impl<T> From<T> for T".to_owned()), "{blanket:?}");
    assert!(blanket.iter().any(|it| it.contains("Into<U> for T")), "{blanket:?}");
    assert!(!blanket.iter().any(|it| it.contains("IntoIterator")), "{blanket:?}");
    let raw = item(fixture, "fixture::Raw");
    let auto: Vec<String> = raw.impls.auto.iter().map(|it| it.header.plain()).collect();
    assert!(auto.contains(&"impl !Send for Raw".to_owned()), "{auto:?}");
    assert!(auto.contains(&"impl !Sync for Raw".to_owned()), "{auto:?}");
    let wraps = item(fixture, "fixture::Wraps");
    let auto: Vec<String> = wraps.impls.auto.iter().map(|it| it.header.plain()).collect();
    assert!(auto.contains(&"impl<T> !Send for Wraps<T>".to_owned()), "{auto:?}");
    assert!(
        auto.contains(&"impl<T> Sync for Wraps<T>\nwhere\n    T: Sync,".to_owned()),
        "{auto:?}"
    );

    let kind = item(fixture, "fixture::Kind");
    let variants: Vec<&str> = kind.variants.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(variants, ["A", "B", "C"]);
    assert_eq!(kind.variants[2].fields[0].name, "x");
    let shape_impl = kind
        .impls
        .traits
        .iter()
        .find(|it| it.trait_name.as_deref() == Some("Shape"))
        .expect("impl Shape");
    let names: Vec<&str> = shape_impl.items.iter().map(|it| it.name.as_str()).collect();
    for name in ["Out", "SIDES", "area", "name"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    assert!(shape_impl.items.iter().find(|it| it.name == "name").unwrap().provided);

    let shape = item(fixture, "fixture::Shape");
    let implementors: Vec<String> =
        shape.impls.implementors.iter().map(|it| it.header.plain()).collect();
    assert_eq!(implementors, ["impl Shape for Kind"]);
    let written: Vec<&str> =
        shape.impls.implementors[0].items.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(written, ["Out", "SIDES"], "an implementor shows its types and constants");
    let members: Vec<&str> = shape.trait_items.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(members, ["Out", "SIDES", "area", "name"]);

    let square = item(fixture, "fixture::shapes::Square");
    assert_eq!(square.impls.inherent[0].items[0].name, "SIDE");
}

#[test]
fn provided_methods_that_cannot_apply_are_left_out() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    let members = |path: &str| -> Vec<String> {
        let each = item(fixture, path)
            .impls
            .traits
            .iter()
            .find(|it| it.trait_name.as_deref() == Some("Each"));
        each.expect("impl Each").items.iter().map(|it| it.name.clone()).collect()
    };
    assert_eq!(members("fixture::Squares"), ["Item", "borrowed"], "a square is not Copy");
    assert_eq!(members("fixture::Finds"), ["Item", "borrowed"], "nor through `A: Clone`");
    assert_eq!(members("fixture::Borrows"), ["Item", "borrowed"], "nor through lifetimes");
    assert_eq!(
        members("fixture::Helpers"),
        ["Item", "copied", "least", "borrowed"],
        "dep may make a helper Copy"
    );
}

#[test]
fn notes() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    let old = item(fixture, "fixture::old").notes.deprecated.clone().expect("deprecated");
    assert_eq!(old.since.as_deref(), Some("1.0"));
    assert_eq!(old.note.as_deref(), Some("use `free`"));
    assert!(item(fixture, "fixture::important").notes.must_use.is_some());
    assert!(item(fixture, "fixture::ffi").notes.is_unsafe);
}

#[test]
fn intra_doc_links_resolve() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    let root = fixture.root();
    assert_eq!(page(link(root, "`Thing`")), "fixture/struct.Thing.html");
    assert_eq!(page(link(root, "`shapes::Square`")), "fixture/shapes/struct.Square.html");
    let thing = item(fixture, "fixture::Thing");
    assert_eq!(page(link(thing, "Self::new")), "fixture/struct.Thing.html#method.new");
    assert_eq!(page(link(thing, "`Kind::A`")), "fixture/enum.Kind.html#variant.A");
    assert_eq!(page(link(thing, "`free()`")), "fixture/fn.free.html");
    // An item the crate inlined links to its page there, another crate's to its own.
    assert_eq!(page(link(thing, "dep::Helper")), "fixture/struct.Helper.html");
    assert_eq!(page(link(thing, "dep::Deep")), "dep/struct.Deep.html");
    assert_eq!(page(link(thing, "`Option`")), "core/option/enum.Option.html");
    assert_eq!(page(link(thing, "Shape")), "fixture/trait.Shape.html");
}

#[test]
fn unresolved_links_warn() {
    let (_db, model) = load();
    let messages: Vec<&str> = model.warnings.iter().map(|it| it.message.as_str()).collect();
    assert!(
        messages.iter().any(|it| it.contains("unresolved link to `missing_item`")),
        "{messages:?}"
    );
    assert!(messages.iter().any(|it| it.contains("unresolved link to `Nope`")), "{messages:?}");
    assert!(messages.iter().any(|it| it.contains("unresolved link to `Loud`")), "{messages:?}");
    assert!(!messages.iter().any(|it| it.contains("`Quiet`")), "allowed: {messages:?}");
    assert!(messages.iter().any(|it| it.contains("`Homegrown`")), "{messages:?}");
    assert!(!messages.iter().any(|it| it.contains("`Foreign`")), "another's macro: {messages:?}");
    assert!(model.warnings.iter().all(|it| it.krate == "fixture"), "dependencies do not warn");
}

#[test]
fn writes_pages_sources_and_search() {
    let (_db, model) = load();
    let out = std::env::temp_dir().join(format!("blaze-doc-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let written = crate::html::write(&model, &out).expect("written");
    assert!(written.pages > 20, "{written:?}");
    assert!(written.source_pages >= 3, "{written:?}");
    for path in [
        "fixture/index.html",
        "fixture/struct.Thing.html",
        "fixture/trait.Shape.html",
        "fixture/macro.make.html",
        "fixture/shapes/index.html",
        "fixture/shapes/struct.Square.html",
        "fixture/sidebar-items.js",
        "dep/struct.Helper.html",
        "src/fixture/lib.rs.html",
        "search-index/fixture.js",
        "crates.js",
        "fixture/all.html",
    ] {
        assert!(out.join(path).is_file(), "{path} written");
    }
    let thing = std::fs::read_to_string(out.join("fixture/struct.Thing.html")).unwrap();
    assert!(thing.contains("id=\"method.new\""), "a member anchor");
    assert!(
        thing.contains("href=\"../fixture/struct.Helper.html\" title=\"struct fixture::Helper\""),
        "an inlined item's page"
    );
    assert!(
        thing.contains("href=\"../dep/struct.Deep.html\" title=\"struct dep::deep::Deep\""),
        "a dependency's page, titled where the item is declared"
    );
    assert!(thing.contains("https://doc.rust-lang.org/nightly/core/option/enum.Option.html"));
    let shape = std::fs::read_to_string(out.join("fixture/trait.Shape.html")).unwrap();
    for id in [
        "required-associated-types",
        "associated-constants",
        "required-methods",
        "provided-methods",
    ] {
        assert!(
            shape.contains(&format!("<h2 id=\"{id}\" class=\"section-header\">")),
            "{id} section"
        );
    }
    assert!(!shape.contains("id=\"provided-associated-types\""), "no provided associated type");
    let index = text(&std::fs::read_to_string(out.join("fixture/index.html")).unwrap());
    assert!(index.contains("let thing = fixture::Thing::new(1);"), "the example is there: {index}");
    assert!(!index.contains("let hidden"), "hidden example lines are not");
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn a_crate_not_documented_is_linked_at_its_html_root_url() {
    let (_db, model) = load_with(true, false);
    let out = std::env::temp_dir().join(format!("blaze-doc-test-remote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    crate::html::write(&model, &out).expect("written");
    assert!(!out.join("dep").exists(), "--no-deps");
    let thing = std::fs::read_to_string(out.join("fixture/struct.Thing.html")).unwrap();
    assert!(
        thing.contains(
            "href=\"https://docs.rs/dep/1.2.0/dep/deep/struct.Deep.html\" title=\"struct dep::deep::Deep\""
        ),
        "where it is declared, which its docs redirect from"
    );
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn a_dependency_of_tests_alone_is_not_documented() {
    let (_db, model) = load_with(false, true);
    let names: Vec<&str> = model.crates.iter().map(|it| it.name.as_str()).collect();
    assert!(!names.contains(&"dep"), "{names:?}");
}

/// The text of an HTML page, its tags dropped.
fn text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[test]
fn impl_ids_follow_rustdoc() {
    let id = |text: &str| {
        let mut code = crate::model::Code::default();
        code.text(text);
        crate::collect::impl_anchor(&code)
    };
    assert_eq!(
        id("impl<'a, 'h, A: Debug> Debug for FindIter<'a, 'h, A>"),
        "impl-Debug-for-FindIter%3C'a,+'h,+A%3E"
    );
    assert_eq!(id("impl<T> From<T> for T"), "impl-From%3CT%3E-for-T");
    assert_eq!(id("impl<T> !Send for Wraps<T>"), "impl-Send-for-Wraps%3CT%3E");
    assert_eq!(id("impl<F: Fn() -> u8> Run<F>"), "impl-Run%3CF%3E");
    assert_eq!(
        id("impl<'de: 'a, 'a> Deserialize<'de> for &'a [u8]"),
        "impl-Deserialize%3C'de%3E-for-%26%5Bu8%5D"
    );
    assert_eq!(
        id("impl From<Error> for Box<dyn Error + Send>"),
        "impl-From%3CError%3E-for-Box%3Cdyn+Error+%2B+Send%3E"
    );
    assert_eq!(
        id("impl<'a> Iterator for Iter<'a>\nwhere\n    'a: 'static,"),
        "impl-Iterator-for-Iter%3C'a%3E"
    );
}

#[test]
fn deref_methods_follow_the_chain() {
    let (_db, model) = load();
    let fixture = krate(&model, "fixture");
    let bytes = item(fixture, "fixture::Bytes");
    let sections: Vec<(String, Vec<&str>)> = bytes
        .impls
        .deref
        .iter()
        .map(|it| (it.target.plain(), it.members.iter().map(|m| m.name.as_str()).collect()))
        .collect();
    assert_eq!(
        sections,
        [("Chain".to_owned(), vec!["read"]), ("[u8]".to_owned(), vec!["len", "iter"])]
    );
    assert_eq!(bytes.impls.deref[1].anchor, "deref-methods-%5Bu8%5D");
}

#[test]
fn one_crate_of_a_name_is_documented_as_cargo_picks_it() {
    let reached = |crates: &[(&str, u32, &str, bool)]| {
        crates
            .iter()
            .map(|&(name, krate, version, target)| crate::Reached {
                name: name.to_owned(),
                krate,
                version: version.to_owned(),
                target,
            })
            .collect()
    };
    let kept: Vec<u32> = crate::one_per_name(reached(&[
        ("bitflags", 1, "1.3.2", true),
        ("syn", 2, "2.0.90", false),
        ("quote", 3, "1.0.40", false),
        ("bitflags", 4, "2.6.0", true),
        ("syn", 5, "1.0.109", false),
        ("memchr", 6, "2.7.4", false),
        ("memchr", 7, "2.6.0", true),
        ("rand", 8, "0.9.0-beta.1", true),
        ("rand", 9, "0.9.0", true),
    ]))
    .into_iter()
    .map(|it| it.krate)
    .collect();
    // Both `syn`s are the host's (a proc macro's), so neither; the older `memchr` is the only
    // one built for the target.
    assert_eq!(kept, [4, 3, 7, 9]);
}

#[test]
fn a_crate_only_a_crate_left_out_depends_on_is_left_out() {
    // 0 is asked for; 1 is the older of two crates of a name, left out, and 3 only it uses.
    let deps = |it: u32| match it {
        0 => vec![1, 2],
        1 => vec![3, 4],
        2 => vec![4, 5],
        5 => vec![2],
        _ => vec![],
    };
    let kept = [2, 3, 4, 5].into_iter().collect();
    let mut linked: Vec<u32> = crate::still_reached(&[0], &kept, deps).into_iter().collect();
    linked.sort();
    assert_eq!(linked, [2, 4, 5]);
}
