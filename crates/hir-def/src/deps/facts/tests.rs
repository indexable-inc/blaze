//! Tests of an item's facts kept as entries (rows B, C and E of docs/v2/plan/design/restore.md,
//! "Items read in place"): each with its control, the cause removed or the edit that must miss.
//! Written by claude-opus-5-5.

use std::{alloc::Global, sync::Arc};

use base_db::{Crate, Db};
use cfg::{CfgAtom, CfgExpr};
use either::Either;
use hir_expand::{
    Lookup, fixture,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use rustc_abi::ExternAbi;
use span::{Edition, SyntaxContext};

use super::{Ctx, In, KEYED_BY_FILE, LAYOUT, LAYOUT_AS, LIVE, Out, RAN, Value};
use crate::{
    AdtId, AssocItemId, AstIdLoc, AttrDefId, HasModule, ModuleDefId,
    attrs::AttrFlags,
    deps::{
        self, Mode, Which,
        tests::{BUILD, Memory as Images, TURN, all},
    },
    item_tree::{RawVisibility, VisibilityExplicitness},
    nameres::{
        assoc::ImplItems,
        crate_def_map,
        frozen::entries::{self, Memory},
    },
    signatures::{
        WrittenSelfView, enum_has_unsafe_fields, extern_block_abi, written_self_view,
        written_visibility,
    },
    test_db::TestDBExt,
};

const CTX: Ctx = Ctx { edition: Edition::CURRENT };

/// `value` written and read back.
fn round_trip<T: Value + PartialEq + std::fmt::Debug>(value: T) {
    let mut out = Out::new();
    assert!(value.put(CTX, &mut out), "{value:?} is kept");
    let bytes = out.bytes();
    let mut input = In(bytes);
    assert_eq!(T::take(CTX, &mut input).unwrap(), value);
    assert!(input.0.is_empty(), "{value:?}: bytes left");
    // Every value says where it ends, so each shorter prefix is refused, never read as some
    // other value.
    for len in 0..bytes.len() {
        assert!(T::take(CTX, &mut In(&bytes[..len])).is_err(), "{value:?} cut to {len} bytes");
    }
}

fn names(texts: &[&str]) -> Vec<Name> {
    texts.iter().map(|it| Name::new_symbol_root(Symbol::intern(it))).collect()
}

/// The refinement: each value a fact holds reads back as itself.
#[test]
fn every_fact_value_reads_back_as_itself() {
    round_trip(true);
    round_trip(false);
    round_trip(7u8);
    round_trip(70_000u32);
    round_trip(Symbol::intern("link_name_value"));
    round_trip(Name::new_symbol_root(Symbol::intern("name")));
    round_trip(Box::<str>::from("use it"));
    round_trip(None::<Symbol>);
    round_trip(Some(Symbol::intern("shiny")));
    round_trip(Some(names(&["a", "b"]).into_boxed_slice()));
    round_trip(vec![1u32, 2, 3].into_boxed_slice());
    let mut set = blaze_hash::HashSet::with_capacity_in(2, Global);
    set.extend([Symbol::intern("fma"), Symbol::intern("avx2")]);
    round_trip(set);
    round_trip(CfgExpr::All(Box::new([
        CfgExpr::Atom(CfgAtom::Flag(Symbol::intern("test"))),
        CfgExpr::Not(Box::new(CfgExpr::Any(Box::new([CfgExpr::Atom(CfgAtom::KeyValue {
            key: Symbol::intern("target_os"),
            value: Symbol::intern("linux"),
        })])))),
        CfgExpr::Invalid,
    ])));
    for &abi in ExternAbi::ALL_VARIANTS {
        round_trip(abi);
    }
    round_trip(WrittenSelfView { fields: names(&["x", "y"]).into_boxed_slice(), mutable: true });
    round_trip(RawVisibility::Public);
    round_trip(RawVisibility::PubCrate);
    round_trip(RawVisibility::PubSelf(VisibilityExplicitness::Explicit));
    let root = SyntaxContext::root(CTX.edition);
    for kind in [PathKind::Plain, PathKind::Super(2), PathKind::Crate, PathKind::Abs] {
        let path = ModPath::from_segments(kind, names(&["inner", "deep"]));
        round_trip(RawVisibility::Module(path, VisibilityExplicitness::Explicit, root));
        round_trip(ModPath::from_segments(kind, names(&["opaque", "Alias"])));
    }
    round_trip(Some((1u8, 4u8)));
    round_trip(vec![AttrFlags::IS_DOC_HIDDEN | AttrFlags::IS_DEPRECATED, AttrFlags::empty()].into_boxed_slice());
    let repr = |int, scalable| rustc_abi::ReprOptions {
        int,
        align: rustc_abi::Align::from_bytes(16).ok(),
        pack: Some(rustc_abi::Align::ONE),
        flags: rustc_abi::ReprFlags::IS_C,
        scalable,
        ..rustc_abi::ReprOptions::default()
    };
    round_trip(rustc_abi::ReprOptions::default());
    round_trip(repr(Some(rustc_abi::IntegerType::Pointer(true)), None));
    let fixed = rustc_abi::IntegerType::Fixed(rustc_abi::Integer::I128, false);
    round_trip(repr(Some(fixed), Some(rustc_abi::ScalableElt::ElementCount(4))));
    round_trip(repr(None, Some(rustc_abi::ScalableElt::Container)));
    let lib = |kind| crate::attrs::NativeLib {
        name: Symbol::intern("ssl"),
        kind,
        verbatim: true,
        whole_archive: false,
        bundle: true,
        as_needed: Some(false),
        import_name_type: Some(Symbol::intern("noprefix")),
    };
    let flag = CfgExpr::Atom(CfgAtom::Flag(Symbol::intern("test")));
    round_trip(
        vec![
            (lib(crate::attrs::NativeLibKind::Static), Some(flag)),
            (lib(crate::attrs::NativeLibKind::Other(Symbol::intern("link-arg"))), None),
        ]
        .into_boxed_slice(),
    );
}

/// The control of the refinement: what a fact cannot hold is not kept (a visibility of a
/// context other than its file's root), and bytes that are not a value are refused by what
/// they are: an unknown tag, an ABI past the list, a count the bytes cannot hold.
#[test]
fn a_value_of_another_context_or_bytes_of_none_are_refused() {
    let other = SyntaxContext::root(Edition::Edition2015);
    assert_ne!(other, SyntaxContext::root(CTX.edition));
    let path = ModPath::from_segments(PathKind::Crate, names(&["inner"]));
    let foreign = RawVisibility::Module(path, VisibilityExplicitness::Explicit, other);
    assert!(!foreign.put(CTX, &mut Out::new()));

    let said = CfgExpr::take(CTX, &mut In(&[9])).unwrap_err();
    assert!(said.contains("a cfg of tag 9"), "{said}");
    let said = ExternAbi::take(CTX, &mut In(&[255])).unwrap_err();
    assert!(said.contains("an ABI of tag 255"), "{said}");
    let said = RawVisibility::take(CTX, &mut In(&[7])).unwrap_err();
    assert!(said.contains("a visibility of tag 7"), "{said}");
    let said = rustc_abi::Align::take(CTX, &mut In(&[64])).unwrap_err();
    assert!(said.contains("an alignment of tag 64"), "{said}");
    let said = rustc_abi::ReprOptions::take(CTX, &mut In(&[0, 0, 0, 0xFF, 0])).unwrap_err();
    assert!(said.contains("repr flags of tag 255"), "{said}");
    let said = <Box<[u32]>>::take(CTX, &mut In(&u32::MAX.to_le_bytes())).unwrap_err();
    assert!(said.contains("things in 0 bytes"), "{said}");
}

/// A dependency with an item for each fact, and a dependent.
const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
fn main() {}
//- /lib.rs crate:lib new_source_root:library
extern crate self as me;
pub struct P { #[doc(hidden)] x: u8, y: u8 }
impl P {
    pub fn viewed(&self.{x}) -> u8 { 0 }
    pub fn whole(&mut self) {}
}
pub enum Guarded { A { unsafe f: u8 }, B }
macro_rules! make { ($n:ident) => { #[must_use = "made"] pub struct $n; } }
make!(MadeA);
make!(MadeB);
#[macro_export]
macro_rules! mac { () => {} }
#[link(name = "m", wasm_import_module = "env")]
extern "C" {
    #[link_name = "real"]
    pub fn alias();
}
#[link(name = "only_when_test", cfg(test))]
#[link(name = "always", kind = "static")]
extern "C" {}
#[repr(C, align(8))]
pub struct Reprd(u8);
#[rust_analyzer::define_opaque(Alias, crate::inner::Other, super::Up)]
#[sanitize(address = "off")]
pub fn opaque() {}
#[must_use = "use it"]
pub fn used() -> u32 { 0 }
#[unstable(feature = "shiny", issue = "none")]
pub fn shiny() {}
#[target_feature(enable = "avx2,fma")]
pub unsafe fn fast() {}
#[rustc_must_implement_one_of(a, b)]
pub trait One { fn a(&self) {} fn b(&self) {} }
#[cfg(all())]
pub fn kept() {}
#[export_name = "exported"]
pub fn exp() {}
#[rustc_legacy_const_generics(1)]
pub fn legacy<const N: u32>(a: u32) {}
pub(crate) struct Hidden;
#[must_use]
pub enum E { A, B(u8) }
pub mod inner {
    pub(in crate::inner) struct Deep;
    pub(super) struct Up;
}
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// Every fact of `lib` a dependent's build asks once its names are found, as text: equal for
/// equal answers of two databases of one text. The ids are its def map's and its impls'
/// entries', so nothing here is found through syntax; a method's self view reads its
/// signature, which a served crate reads in place from its image.
fn ask(db: &Db) -> Vec<String> {
    let lib = crate_named(db, "lib");
    let mut said = Vec::new();
    let mut say = |what: &str, it: &dyn std::fmt::Debug| said.push(format!("{what} {it:?}"));
    for (module, data) in crate_def_map(db, lib).modules(db) {
        let scope = data.scope(db);
        for def in scope.declarations() {
            let owner = match def {
                ModuleDefId::FunctionId(it) => {
                    let owner = AttrDefId::FunctionId(it);
                    say("link_name", &AttrFlags::link_name(db, owner));
                    say("export_name", &AttrFlags::export_name(db, owner));
                    let mut features: Vec<&str> =
                        AttrFlags::target_features(db, it).iter().map(Symbol::as_str).collect();
                    features.sort_unstable();
                    say("target_feature", &features);
                    say("legacy", &AttrFlags::legacy_const_generic_indices(db, it));
                    say("vis", &written_visibility(db, it.lookup(db).ast_id(), it.module(db)));
                    say("define_opaque", &AttrFlags::define_opaque(db, it));
                    say("sanitize", &crate::attrs::disabled_sanitizers(db, it));
                    owner
                }
                ModuleDefId::AdtId(it) => {
                    say("repr", &AttrFlags::repr(db, it));
                    let vis = match it {
                        AdtId::StructId(it) => {
                            let parent = crate::VariantId::StructId(it);
                            let local_id = crate::LocalFieldId::from_raw(la_arena::RawIdx::from_u32(0));
                            let flags = AttrFlags::query_field(db, crate::FieldId { parent, local_id });
                            say("first field doc hidden", &flags.contains(AttrFlags::IS_DOC_HIDDEN));
                            written_visibility(db, it.lookup(db).ast_id(), module)
                        }
                        AdtId::EnumId(it) => {
                            say("unsafe fields", &enum_has_unsafe_fields(db, it));
                            written_visibility(db, it.lookup(db).ast_id(), module)
                        }
                        AdtId::UnionId(it) => written_visibility(db, it.lookup(db).ast_id(), module),
                    };
                    say("vis", &vis);
                    AttrDefId::AdtId(it)
                }
                ModuleDefId::TraitId(it) => {
                    say("one_of", &AttrFlags::must_implement_one_of(db, it));
                    say("vis", &written_visibility(db, it.lookup(db).ast_id(), module));
                    AttrDefId::TraitId(it)
                }
                ModuleDefId::MacroId(it) => {
                    say("macro", &it.name(db));
                    continue;
                }
                _ => continue,
            };
            let flags = AttrFlags::query(db, owner);
            say("must_use", &AttrFlags::must_use_message(db, owner));
            say("unstable", &flags.unstable_feature(db, owner));
            say("cfg", &flags.cfgs(db, Either::Left(owner)));
        }
        for (_, item) in scope.macros() {
            say("macro", &item.def.name(db));
        }
        for it in scope.extern_crate_decls() {
            say("extern crate", &it.alias_or_name(db));
        }
        for it in scope.extern_blocks() {
            say("abi", &extern_block_abi(db, it));
            say("wasm", &AttrFlags::wasm_import_module(db, it));
            let libs: Vec<&str> =
                AttrFlags::native_libs(db, it).iter().map(|lib| lib.name.as_str()).collect();
            say("libs", &libs);
        }
        for it in scope.impls() {
            for (name, item) in &ImplItems::of(db, it).0.items {
                if let AssocItemId::FunctionId(it) = *item {
                    say(&format!("self view of {}", name.as_str()), &written_self_view(db, it));
                }
            }
        }
    }
    said.sort_unstable();
    said
}

/// A database of `text` whose entries are `store`.
fn keeping(text: &str, store: &Arc<Memory>) -> Db {
    let mut db = fixture::db(text).into_db();
    // clone: a test's handle of its store
    entries::set(&mut db, store.clone());
    db
}

/// The images of `text`'s crates, written by [`deps::store`] from a database over `store` that
/// resolves them from source: what the driver does when a crate is first resolved, so its
/// facts are kept in `store` then (`facts::keep_all`, "Written when" in restore.md).
fn imaged(text: &str, store: &Arc<Memory>) -> Images {
    let images = Images::default();
    let mut db = keeping(text, store);
    let crates = all(&db);
    let served = deps::serve(&mut db, &crates, BUILD, &images, Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    let stored = deps::store(&db, &served, &images, Which::All).unwrap();
    assert_eq!(stored.refused, Vec::new());
    assert!(stored.written.contains(&crate_named(&db, "lib")), "lib's image written");
    images
}

/// A database of `text` whose entries are `store` and that is served `lib` from `images`: its
/// def map and signatures read in place, and every fact of it from `store`.
fn serving(text: &str, store: &Arc<Memory>, images: &Images) -> Db {
    let mut db = keeping(text, store);
    let crates = all(&db);
    let served = deps::serve(&mut db, &crates, BUILD, images, Mode::Serve).unwrap();
    assert!(served.hits.contains(&crate_named(&db, "lib")), "lib read from its image");
    // What the databases before this one counted.
    _ = store.take();
    db
}

/// A database of `text` served `lib` from the image a database of `text` over `store` wrote,
/// as a dependency is served in a later session.
fn served(text: &str, store: &Arc<Memory>) -> Db {
    let images = imaged(text, store);
    serving(text, store, &images)
}

/// The queries among `ran` that make syntax, an item tree or an expansion, by the names the
/// run log gives them.
fn made_syntax(ran: &[String]) -> Vec<&String> {
    const MAKERS: &[&str] =
        &["parse", "item_tree", "macro_expansion", "outline", "expander", "macro_arg", "ast_id_map"];
    ran.iter().filter(|name| MAKERS.iter().any(|it| name.contains(it))).collect()
}

/// The facts of [`FIXTURE`]'s `lib`, kept by a database that made them.
fn written() -> (Arc<Memory>, Vec<String>) {
    let store = Memory::new();
    let want = ask(&keeping(FIXTURE, &store));
    _ = store.take();
    (store, want)
}

/// The bar of row B and C: a dependency served from records, whose facts are kept, answers
/// every fact as the database that made them did, and no file of it is parsed, outlined,
/// item-treed or expanded to do it: a method's self view and an enum's unsafe fields among
/// them, with the signatures they read served in place.
#[test]
fn a_served_crate_answers_every_fact_with_no_syntax() {
    let _turn = TURN.lock();
    let (store, want) = written();
    // A struct a macro wrote among them: its facts are found under its expansion's key, with
    // no expansion (row D).
    let made = want.iter().filter(|it| it.starts_with("must_use Some(Some(\"made\"))")).count();
    assert_eq!(made, 2, "both expansions' structs asked: {want:?}");
    for fact in [
        "self view of viewed Some",
        "self view of whole None",
        "unsafe fields true",
        "first field doc hidden true",
        // The library under a `cfg` the crate lacks is kept in the fact and left out on read.
        "libs [\"always\"]",
        "repr Some(",
    ] {
        assert!(want.iter().any(|it| it.starts_with(fact)), "{fact} asked: {want:?}");
    }
    for (fact, none) in [("define_opaque [", "define_opaque []"), ("sanitize ", "sanitize 0")] {
        let found = want.iter().any(|it| it.starts_with(fact) && it != none);
        assert!(found, "a written {fact} asked: {want:?}");
    }
    assert!(want.len() > 30, "{} facts asked", want.len());
    let db = served(FIXTURE, &store);
    RAN.set(0);
    let mut got = Vec::new();
    let ran = db.executed_in_read_order(|| got = ask(&db));
    assert_eq!(got, want);
    assert_eq!(made_syntax(&ran), Vec::<&String>::new(), "all that ran: {ran:?}");
    assert_eq!(RAN.get(), 0, "every fact found");
    assert_eq!(store.take().misses, 0);
}

/// The control: the cause removed. The same asks of the same served crate with the facts
/// read live (one reader put back on its syntax) cut the file's outline and number its
/// syntax, and the count above catches it.
#[test]
fn with_facts_read_live_the_same_asks_make_syntax() {
    let _turn = TURN.lock();
    let (store, want) = written();
    let db = served(FIXTURE, &store);
    LIVE.set(true);
    let mut got = Vec::new();
    let ran = db.executed_in_read_order(|| got = ask(&db));
    LIVE.set(false);
    assert_eq!(got, want);
    assert!(!made_syntax(&ran).is_empty(), "nothing made syntax: {ran:?}");
}

/// A crate `lib` of `count` structs, each `#[must_use]` with a message, the one at `edited`
/// with another.
fn sized(count: usize, edited: Option<usize>) -> String {
    let mut text = String::from("//- /main.rs crate:main deps:lib\nfn main() {}\n");
    text.push_str("//- /lib.rs crate:lib\n");
    for at in 0..count {
        let message = if edited == Some(at) { "edited" } else { "kept" };
        text.push_str(&format!("#[must_use = \"{message} {at}\"]\npub struct S{at};\n"));
    }
    text
}

/// The `#[must_use]` message of each struct of `lib`, in its order.
fn messages(db: &Db) -> Vec<String> {
    let lib = crate_named(db, "lib");
    let def_map = crate_def_map(db, lib);
    let mut said: Vec<String> = def_map
        .modules(db)
        .flat_map(|(_, data)| data.scope(db).declarations())
        .filter_map(|def| match def {
            ModuleDefId::AdtId(it) => {
                let owner = AttrDefId::AdtId(it);
                Some(format!("{:?}", AttrFlags::must_use_message(db, owner)))
            }
            _ => None,
        })
        .collect();
    said.sort_unstable();
    said
}

const N: usize = 12;

/// Made again after one struct of N is edited: under each item's own key, the edited one's.
fn made_after_one_edit() -> u64 {
    let store = Memory::new();
    _ = messages(&keeping(&sized(N, None), &store));
    let after = sized(N, Some(N / 2));
    let want = messages(&fixture::db(&after).into_db());
    let db = keeping(&after, &store);
    RAN.set(0);
    assert_eq!(messages(&db), want);
    RAN.get()
}

/// Rule 65 by counts: one item of N edited makes one fact again; the N-1 others are found
/// under their own keys.
#[test]
fn one_item_of_n_edited_makes_its_fact_alone() {
    assert_eq!(made_after_one_edit(), 1);
}

/// The control: a key of the file (as a crate's or a file's key would be) misses every fact
/// of the file on one edit, so the count above tells a per-item key from a file's.
#[test]
fn keyed_by_file_one_edit_makes_every_fact_again() {
    KEYED_BY_FILE.set(true);
    let made = made_after_one_edit();
    KEYED_BY_FILE.set(false);
    assert_eq!(made, N as u64);
}

/// A crate `lib` of `count` structs a `macro_rules!` writes, one call each, its message
/// `message`; the call at `renamed` names its struct `R` in place of `S`.
fn made_by_calls(count: usize, renamed: Option<usize>, message: &str) -> String {
    let mut text = String::from("//- /main.rs crate:main deps:lib\nfn main() {}\n");
    text.push_str("//- /lib.rs crate:lib\n");
    text.push_str(&format!(
        "macro_rules! make {{ ($n:ident) => {{ #[must_use = \"{message}\"] pub struct $n; }} }}\n"
    ));
    for at in 0..count {
        let name = if renamed == Some(at) { "R" } else { "S" };
        text.push_str(&format!("make!({name}{at});\n"));
    }
    text
}

/// Made again after `before` is edited to `after`, each a text of [`made_by_calls`].
fn made_after(before: &str, after: &str) -> u64 {
    let store = Memory::new();
    _ = messages(&keeping(before, &store));
    let want = messages(&fixture::db(after).into_db());
    assert_eq!(want.len(), N, "every call's struct asked: {want:?}");
    let db = keeping(after, &store);
    RAN.set(0);
    assert_eq!(messages(&db), want);
    RAN.get()
}

/// Row D by counts: an item a macro wrote is keyed by its expansion's inputs
/// (`stored::expansion::key`), so one call of N edited makes its own expansion's fact again
/// and the N-1 others are found, though every expansion's text is a function of the same
/// definition and file.
#[test]
fn one_call_of_n_edited_makes_its_expansions_fact_alone() {
    let made = made_after(&made_by_calls(N, None, "made"), &made_by_calls(N, Some(N / 2), "made"));
    assert_eq!(made, 1);
}

/// The control: the definition, which every expansion's key covers, edited makes every
/// expansion's fact again, so the count above tells a key of the call's inputs from one that
/// misses them.
#[test]
fn the_definition_edited_makes_every_expansions_fact_again() {
    let made = made_after(&made_by_calls(N, None, "made"), &made_by_calls(N, None, "made again"));
    assert_eq!(made, N as u64);
}

/// A store of facts of a previous layout: each is a miss under this layout's key and made
/// again, never read as this layout's value and never refused. The control: the same store
/// read at the layout it was written with finds every one.
#[test]
fn facts_of_a_previous_layout_are_made_again() {
    let _turn = TURN.lock();
    let read_with = |written_as: u32| {
        let store = Memory::new();
        LAYOUT_AS.set(Some(written_as));
        let want = ask(&keeping(FIXTURE, &store));
        let images = imaged(FIXTURE, &store);
        LAYOUT_AS.set(None);
        let db = serving(FIXTURE, &store, &images);
        RAN.set(0);
        assert_eq!(ask(&db), want);
        RAN.get()
    };
    let previous = read_with(LAYOUT - 1);
    assert!(previous > 10, "{previous} facts made again");
    assert_eq!(read_with(LAYOUT), 0);
}
