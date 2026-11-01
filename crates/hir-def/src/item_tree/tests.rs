use base_db::EditionedFileId;
use blaze_format::Persist;
use expect_test::{Expect, expect};
use hir_expand::HirFileId;
use span::Edition;
use hir_expand::fixture;

use crate::{
    item_tree::{ItemTree, ModItemKind, RawVisibility},
    test_db::TestDB,
};

fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
    let fx = fixture::db(ra_fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    let item_tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    let pretty = item_tree.pretty_print(&db, Edition::CURRENT);
    expect.assert_eq(&pretty);
    crate::item_tree::persist::assert_round_trip(&item_tree);
    assert_assembled_is_whole(&db, file_id, ra_fixture);
}

/// The tree of `file` made from its kept one ([`super::items`]) is the one it lowers to whole,
/// to the byte, and so is the tree [`crate::file_item_tree`] gives. Made three times, so each
/// of a first lowering, a keyed one and one from the kept tree is checked, whichever the slot
/// held.
pub(super) fn assert_assembled_is_whole(db: &TestDB, file: EditionedFileId, what: &str) {
    let krate = fixture::test_crate(db);
    let whole = super::lower_whole(db, file.into(), krate);
    for _ in 0..3 {
        let made = super::items::file_item_tree(db, file, krate);
        assert_eq!(*made, whole, "the tree of\n{what}");
        assert_eq!(blaze_format::to_bytes(&*made), blaze_format::to_bytes(&whole), "{what}");
    }
    match crate::file_item_tree(db, file.into(), krate) {
        tree if whole.is_empty() => assert!(tree.is_empty(), "{what}"),
        tree => assert_eq!(*tree, whole, "{what}"),
    }
}

#[test]
fn imports() {
    check(
        r#"
//! file comment
#![no_std]
//! another file comment

extern crate self as renamed;
pub(super) extern crate bli;

pub use crate::path::{nested, items as renamed, Trait as _};
use globs::*;

/// docs on import
use crate::{A, B};

use a::{c, d::{e}};
        "#,
        expect![[r#"
            #![no_std]

            // AstId: ExternCrate[3133, 0]
            pub(self) extern crate self as renamed;

            // AstId: ExternCrate[0ABE, 0]
            pub(in super) extern crate bli;

            // AstId: Use[4AF9, 0]
            pub use crate::path::{nested, items as renamed, Trait as _};

            // AstId: Use[4AF9, 1]
            pub(self) use globs::*;

            // AstId: Use[4AF9, 2]
            pub(self) use crate::{A, B};

            // AstId: Use[4AF9, 3]
            pub(self) use a::{c, d::{e}};
        "#]],
    );
}

#[test]
fn extern_blocks() {
    check(
        r#"
#[on_extern_block]
extern "C" {
    #[on_extern_type]
    type ExType;

    #[on_extern_static]
    static EX_STATIC: u8;

    #[on_extern_fn]
    fn ex_fn();
}
        "#,
        expect![[r#"
            #![feature(no_core)]
            #![no_core]

            #[on_extern_block]
            // AstId: ExternBlock[4AF9, 0]
            extern {
                #[on_extern_type]
                // AstId: TypeAlias[0116, 0]
                pub(self) type ExType;

                #[on_extern_static]
                // AstId: Static[26B2, 0]
                pub(self) static EX_STATIC = _;

                #[on_extern_fn]
                // AstId: Fn[7AA2, 0]
                pub(self) fn ex_fn;
            }
        "#]],
    );
}

#[test]
fn adts() {
    check(
        r#"
struct Unit;

#[derive(Debug)]
struct Struct {
    /// fld docs
    fld: (),
}

struct Tuple(#[attr] u8);

union Ize {
    a: (),
    b: (),
}

enum E {
    /// comment on Unit
    Unit,
    /// comment on Tuple
    Tuple(u8),
    Struct {
        /// comment on a: u8
        a: u8,
    }
}
        "#,
        expect![[r#"
            #![feature(no_core)]
            #![no_core]

            // AstId: Struct[7CA4, 0]
            pub(self) struct Unit { ... }

            #[derive(Debug)]
            // AstId: Struct[24DC, 0]
            pub(self) struct Struct { ... }

            // AstId: Struct[2B78, 0]
            pub(self) struct Tuple { ... }

            // AstId: Union[D936, 0]
            pub(self) union Ize { ... }

            // AstId: Enum[4BCF, 0]
            pub(self) enum E { ... }
        "#]],
    );
}

#[test]
fn misc() {
    check(
        r#"
pub static mut ST: () = ();

const _: Anon = ();

#[attr]
fn f(#[attr] arg: u8, _: ()) {
    #![inner_attr_in_fn]
}

trait Tr: SuperTrait + 'lifetime {
    type Assoc: AssocBound = Default;
    fn method(&self);
}
        "#,
        expect![[r#"
            #![feature(no_core)]
            #![no_core]

            // AstId: Static[3B27, 0]
            pub static ST = _;

            // AstId: Const[09E8, 0]
            pub(self) const _ = _;

            #[attr]
            #[inner_attr_in_fn]
            // AstId: Fn[D606, 0]
            pub(self) fn f;

            // AstId: Trait[C96A, 0]
            pub(self) trait Tr { ... }
        "#]],
    );
}

#[test]
fn modules() {
    check(
        r#"
/// outer
mod inline {
    //! inner

    use super::*;

    fn fn_in_module() {}
}

mod outline;
        "#,
        expect![[r##"
            #![feature(no_core)]
            #![no_core]

            // AstId: Module[D85F, 0]
            pub(self) mod inline {
                // AstId: Use[4AF9, 0]
                pub(self) use super::*;

                // AstId: Fn[E3BC, 0]
                pub(self) fn fn_in_module;
            }

            // AstId: Module[83E7, 0]
            pub(self) mod outline;
        "##]],
    );
}

#[test]
fn macros() {
    check(
        r#"
macro_rules! m {
    () => {};
}

pub macro m2() {}

m!();
        "#,
        expect![[r#"
            #![feature(no_core)]
            #![no_core]

            // AstId: MacroRules[2FDA, 0]
            macro_rules! m { ... }

            // AstId: MacroDef[5D7C, 0]
            pub macro m2 { ... }

            // AstId: MacroCall[46AE, 0], SyntaxContextId: ROOT2024, ExpandTo: Items
            m!(...);
        "#]],
    );
}

#[test]
fn pub_self() {
    check(
        r#"
pub(self) struct S;
        "#,
        expect![[r#"
            #![feature(no_core)]
            #![no_core]

            // AstId: Struct[5682, 0]
            pub(self) struct S { ... }
        "#]],
    )
}

#[test]
fn crate_attrs_should_preserve_order() {
    check(
        r#"
//- /main.rs crate:foo crate-attr:no_std crate-attr:features(f16) crate-attr:crate_type="bin"
        "#,
        expect![[r##"
            #![no_std]
            #![features(f16)]
            #![crate_type = "bin"]
        "##]],
    );
}

#[test]
fn crate_attrs_with_disabled_cfg_injected() {
    check(
        r#"
//- /main.rs crate:foo crate-attr:no_std crate-attr:cfg(false) crate-attr:features(f16,f128) crate-attr:crate_type="bin"
        "#,
        expect![[r#"
            #![no_std]
            #![cfg(false)]
        "#]],
    );
}

#[test]
fn crate_attrs_with_disabled_cfg_in_source() {
    check(
        r#"
//- /lib.rs crate:foo crate-attr:no_std
#![cfg(false)]
#![no_core]
        "#,
        // rustc appends `-Zcrate-attr`s to the source's and stops at the first false `cfg`.
        expect![[r#"
            #![cfg(false)]
        "#]],
    );
}

#[test]
fn use_trees_expand_in_order_of_their_syntax() {
    use la_arena::{Idx, RawIdx};

    let fx = fixture::db("use a::{b, c::{d, *}, self, e as f};");
    let file_id = fx.only_file();
    let db = fx.into_db();
    let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    let [item] = tree.top_level_items() else { panic!("one item") };
    let ModItemKind::Use(_, use_item) = tree.index(*item) else { panic!("a `use`") };
    let mut imports = Vec::new();
    use_item.expand(tree, |idx, path, kind, alias| {
        let alias = alias.map(|alias| alias.display(Edition::CURRENT).to_string());
        let path = path.display(&db, Edition::CURRENT);
        imports.push((idx, format!("{path} {kind:?} {alias:?}")));
    });
    let tree_at = |index| Idx::from_raw(RawIdx::from_u32(index));
    assert_eq!(
        imports,
        [
            (tree_at(1), "a::b Plain None".to_owned()),
            (tree_at(3), "a::c::d Plain None".to_owned()),
            (tree_at(4), "a::c Glob None".to_owned()),
            (tree_at(5), "a TypeOnly None".to_owned()),
            (tree_at(6), r#"a::e Plain Some("f")"#.to_owned()),
        ]
    );
}

#[test]
fn struct_constructors_have_the_least_visibility_of_their_fields() {
    use crate::item_tree::{RawVisibility, StructValueNsCtor};

    let fx = fixture::db(
        r#"
pub struct R { a: u8 }
pub(crate) struct U;
pub struct T(pub u8, pub(crate) u16);
pub struct M(pub(in crate::a) u8, pub(in crate::b) u8);
"#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    let ctors: Vec<Option<Vec<RawVisibility>>> = (tree.top_level_items().iter())
        .map(|&item| {
            let ModItemKind::Struct(_, strukt) = tree.index(item) else { panic!("a struct") };
            match tree.value_ns_ctor(strukt) {
                StructValueNsCtor::NoValueNsCtor => None,
                StructValueNsCtor::ValueNsCtorWithVis(vis) => Some(vec![tree[*vis].clone()]),
                StructValueNsCtor::ValueNsCtorWithMinVis(all) => {
                    Some(all.iter().map(|vis| tree[*vis].clone()).collect())
                }
            }
        })
        .collect();
    let pub_crate = Some(vec![RawVisibility::PubCrate]);
    assert_eq!(ctors[..3], [None, pub_crate.clone(), pub_crate]);
    let least = ctors[3].as_ref().expect("a tuple struct's constructor");
    assert_eq!(least.len(), 2);
    assert!(least.iter().all(|vis| matches!(vis, RawVisibility::Module(..))));
}

/// The tree of `file`, owned (decoded from its bytes) so it outlives an edit, and its bytes.
fn owned_tree(db: &TestDB, file: HirFileId) -> (ItemTree, Vec<u8>) {
    let tree = crate::file_item_tree(db, file, fixture::test_crate(db));
    let bytes = blaze_format::to_bytes(tree);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let owned = ItemTree::read(&buffer).and_then(|read| read.decode()).expect("a tree written");
    assert_eq!(owned, *tree);
    if let HirFileId::FileId(file) = file {
        assert_assembled_is_whole(db, file, "a file of the test");
    }
    (owned, bytes)
}

const EDITED: &str = r#"
use std::fmt;
pub struct S(pub u8);
mod m {
    pub fn inner() { let x = 1; }
}
pub fn f(s: S) -> u8 { s.0 }
impl S { fn g(&self) {} }
"#;

#[test]
fn an_edit_inside_a_body_changes_no_byte_of_the_tree() {
    let fx = fixture::db(EDITED);
    let file_id = fx.only_file();
    let mut db = fx.into_db();
    let file: HirFileId = file_id.into();
    let (before, bytes) = owned_tree(&db, file);
    for edited in [
        EDITED.replace("{ s.0 }", "{ let y = s.0 + 1; y * 2 }"),
        EDITED.replace("let x = 1;", "let x = [1, 2, 3].len(); x"),
        EDITED.replace("fn g(&self) {}", "fn g(&self) { loop {} }"),
    ] {
        db.set_file_text(file_id.file_id(&db), &edited);
        let (after, after_bytes) = owned_tree(&db, file);
        assert_eq!(after, before, "{edited}");
        assert_eq!(after_bytes, bytes, "{edited}");
    }
}

#[test]
fn an_edit_of_one_item_changes_only_its_rows() {
    let fx = fixture::db(EDITED);
    let file_id = fx.only_file();
    let mut db = fx.into_db();
    let file: HirFileId = file_id.into();
    let (before, _) = owned_tree(&db, file);
    db.set_file_text(file_id.file_id(&db), &EDITED.replace("pub fn f(", "pub fn renamed("));
    let (after, _) = owned_tree(&db, file);

    let f = (before.top_level().iter().copied())
        .find(|&item| {
            matches!(before.index(item), ModItemKind::Function(_, it) if before[it.name].as_str() == "f")
        })
        .expect("`f` at the top level");
    let ModItemKind::Function(_, it) = after.index(f) else { panic!("`f`'s row is a fn") };
    assert_eq!(after[it.name].as_str(), "renamed");

    assert_eq!(after.top_level(), before.top_level());
    assert_eq!(after.kinds(), before.kinds());
    assert_eq!(after.data(), before.data());
    assert_eq!(after.functions(), before.functions());
    assert_eq!(after.children(), before.children());
    assert_eq!(
        (after.structs(), after.mods(), after.uses()),
        (before.structs(), before.mods(), before.uses())
    );
    for (at, (old, new)) in before.ast_ids().iter().zip(after.ast_ids()).enumerate() {
        if at != f.index as usize {
            assert_eq!(old, new, "the ast id of item {at}, not the one edited");
        }
    }
    let changed: Vec<usize> =
        (0..before.names().len()).filter(|&at| before.names()[at] != after.names()[at]).collect();
    assert_eq!(changed, [it.name.raw as usize], "only the edited item's name");
}

#[test]
fn a_reused_builder_lowers_as_a_fresh_one() {
    let small = "pub fn a() {}\nmod m { struct S; }\nuse x::{y, z};\n";
    let big = r#"
#![no_std]
extern crate core as c;
pub mod outer {
    pub mod inner { pub struct T(pub(in crate::outer) u8, pub(in crate::x) u8); }
    use a::{b::{c, d}, e::*};
}
extern "C" { fn ext(); static S: u8; }
#[cfg(false)] fn gone() {}
macro_rules! m { () => {} }
m!();
"#;
    let lower = |text: &str| {
        let fx = fixture::db(text);
        let file_id = fx.only_file();
        let db = fx.into_db();
        owned_tree(&db, file_id.into()).1
    };
    let first = lower(small);
    lower(big);
    let again = lower(small);
    assert_eq!(again, first, "a tree lowered after a bigger one on the same thread");
}

#[test]
fn constructor_visibilities_keep_the_order_of_their_fields() {
    use crate::item_tree::{RawVisibility, StructValueNsCtor};

    let fx = fixture::db(
        "pub struct M(pub(in crate::b) u8, pub(in crate::a) u8, pub u8, pub(in crate::b) u16);",
    );
    let file_id = fx.only_file();
    let db = fx.into_db();
    let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    let [item] = tree.top_level_items() else { panic!("one item") };
    let ModItemKind::Struct(_, strukt) = tree.index(*item) else { panic!("a struct") };
    let StructValueNsCtor::ValueNsCtorWithMinVis(all) = tree.value_ns_ctor(strukt) else {
        panic!("several visibilities")
    };
    let paths: Vec<String> = (all.iter())
        .map(|vis| match &tree[*vis] {
            RawVisibility::Module(path, _) => path.display(&db, Edition::CURRENT).to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(paths, ["crate::b", "crate::a"]);
}

#[test]
fn attributes_are_found_as_a_search_of_their_owners_finds_them() {
    use crate::item_tree::{AttrsOrCfgRef, ModItemId};

    fn same(a: AttrsOrCfgRef<'_>, b: AttrsOrCfgRef<'_>) -> bool {
        match (a, b) {
            (AttrsOrCfgRef::Enabled { attrs: a }, AttrsOrCfgRef::Enabled { attrs: b }) => {
                (a.as_ptr(), a.len()) == (b.as_ptr(), b.len())
            }
            (AttrsOrCfgRef::CfgDisabled(c, a), AttrsOrCfgRef::CfgDisabled(d, b)) => {
                std::ptr::eq(c, d) && (a.as_ptr(), a.len()) == (b.as_ptr(), b.len())
            }
            _ => false,
        }
    }

    let items: String = (0..200)
        .map(|i| match i % 3 {
            0 => format!("#[a{i}] fn f{i}() {{}}\n"),
            1 => format!("struct S{i};\n"),
            _ => format!("#[cfg(false)] mod m{i} {{ #[b] fn g() {{}} fn h() {{}} }}\n"),
        })
        .collect();
    let fx = fixture::db(&format!("#![c]\n{items}"));
    let file_id = fx.only_file();
    let db = fx.into_db();
    let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    assert!(tree.kinds().len() > 128, "more than two blocks of items");
    for index in 0..tree.kinds().len() as u32 {
        let id = ModItemId::new(index);
        let searched = (tree.attr_owners().binary_search_by_key(&id, |owner| owner.owner).ok())
            .map(|at| tree.attrs_of(&tree.attr_owners()[at]));
        match (tree.attrs(id), searched) {
            (None, None) => {}
            (Some(found), Some(searched)) => assert!(same(found, searched), "item {index}"),
            (found, searched) => panic!("item {index}: {found:?} against {searched:?}"),
        }
    }
    assert!(tree.top_level_attrs().is_some());
    crate::item_tree::persist::assert_round_trip(tree);
}

/// A file with every kind of item, and attributes with no input.
const EVERY_KIND: &str = r#"
#![no_std]
extern crate core as c;
pub use a::{b::{c, d}, e::*, f as g};
#[my_attr] pub(crate) struct S(pub u8, pub(in crate::m) u16);
pub struct R { a: u8 }
pub union U { a: u8 }
pub enum E { A }
pub trait T {}
impl T for S {}
pub type A = S;
pub const C: u8 = 0;
const _: () = ();
pub static ST: u8 = 0;
pub mod m { #[my_attr] pub fn inner() {} mod deeper; }
extern "C" { fn ext(); static X: u8; }
macro_rules! mac { () => {} }
pub macro m2() {}
mac!();
pub fn f() {}
"#;

/// The paths with segments a decode of `tree` reads, of its paths, visibilities and attributes:
/// each a `Vec` of its names (`Code for ModPath` in `crate::hir::persist`).
fn decoded_paths(tree: &ItemTree) -> u64 {
    let vis = tree.vis().iter().filter_map(|vis| match vis {
        RawVisibility::Module(path, _) => Some(path),
        _ => None,
    });
    let attrs = tree.attr_list().iter().map(|attr| &attr.path);
    (tree.paths().iter().chain(vis).chain(attrs)).filter(|path| !path.segments().is_empty()).count()
        as u64
}

/// Reads every item under `items` through the accessors the def map collector uses; returns how
/// many it read.
fn read_all(tree: &ItemTree, items: &[crate::item_tree::ModItemId]) -> usize {
    use crate::item_tree::ModKind;
    use std::hint::black_box;

    let mut read = 0;
    for &item in items {
        read += 1;
        black_box((item.ast_id(tree), tree.attrs(item)));
        match tree.index(item) {
            ModItemKind::Mod(_, m) => {
                black_box((tree.get_name(m.name), &tree[m.visibility]));
                if let ModKind::Inline { items } = tree.mod_kind(m) {
                    read += read_all(tree, items);
                }
            }
            ModItemKind::ExternBlock(_, block) => read += read_all(tree, tree.items(block.children)),
            ModItemKind::Struct(_, s) => {
                black_box((tree.get_name(s.name), &tree[s.visibility], tree.value_ns_ctor(s)));
            }
            ModItemKind::Use(_, u) => {
                black_box((&tree[u.visibility], tree.use_tree(u.use_tree).kind(tree)));
            }
            ModItemKind::ExternCrate(_, e) => {
                black_box((tree.get_name(e.name), tree.import_alias(e.alias)));
            }
            ModItemKind::MacroCall(_, call) => {
                black_box(&tree[call.path]);
            }
            other => {
                black_box(other);
            }
        }
    }
    read
}

#[test]
fn reading_a_tree_allocates_nothing() {
    let fx = fixture::db(EVERY_KIND);
    let file_id = fx.only_file();
    let db = fx.into_db();
    let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    assert_eq!(read_all(tree, tree.top_level_items()), tree.kinds().len(), "every item read");
    blaze_alloc_count::assert_at_most(0, "reading every item of a tree", || {
        std::hint::black_box(read_all(tree, tree.top_level_items()));
        std::hint::black_box(tree.top_level_attrs());
    });
}

#[test]
fn expanding_use_trees_whose_paths_are_interned_allocates_nothing() {
    let fx = fixture::db(EVERY_KIND);
    let file_id = fx.only_file();
    let db = fx.into_db();
    let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    let expand = || {
        let mut imports = 0;
        for &item in tree.top_level_items() {
            if let ModItemKind::Use(_, it) = tree.index(item) {
                it.expand(tree, |idx, path, kind, alias| {
                    std::hint::black_box((idx, path, kind, alias));
                    imports += 1;
                });
            }
        }
        imports
    };
    assert_eq!(expand(), 4, "`a::b::c`, `a::b::d`, `a::e::*` and `a::f as g`");
    blaze_alloc_count::assert_at_most(0, "expanding the use trees again", || expand());
}

#[test]
fn a_warm_builder_lowers_a_file_in_one_allocation() {
    let fx = fixture::db(EVERY_KIND);
    let file_id = fx.only_file();
    let db = fx.into_db();
    let (file, krate) = (HirFileId::from(file_id), fixture::test_crate(&db));
    let cold = super::lower_whole(&db, file, krate);
    let warm = blaze_alloc_count::assert_at_most(1, "lowering on a warm builder", || {
        super::lower_whole(&db, file, krate)
    });
    assert_eq!(warm, cold);
}

#[test]
fn decoding_a_tree_allocates_its_block_and_its_paths() {
    let fx = fixture::db(EVERY_KIND);
    let file_id = fx.only_file();
    let db = fx.into_db();
    let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
    let buffer = blaze_format::Aligned::copy_from(&blaze_format::to_bytes(tree));
    let read = ItemTree::read(&buffer).expect("a tree written");
    assert_eq!(read.decode().expect("a tree written"), *tree);
    let decoded = blaze_alloc_count::assert_at_most(
        1 + decoded_paths(tree),
        "decoding a tree whose names and paths are interned",
        || read.decode().expect("a tree written"),
    );
    assert_eq!(decoded, *tree);
}

#[test]
fn a_builder_after_an_item_of_many_names_lowers_as_a_fresh_one() {
    let small = "pub fn a() {}\nmod m { struct S; }\nuse x::{y, z};\n";
    let names: String = (0..600).map(|i| format!("fn f{i}() {{}}\n")).collect();
    let big = format!("mod many {{\n{names}}}\n");
    let lower = |text: &str| {
        let fx = fixture::db(text);
        let file_id = fx.only_file();
        let db = fx.into_db();
        owned_tree(&db, file_id.into()).1
    };
    let first = lower(small);
    lower(&big);
    assert_eq!(lower(small), first, "a tree lowered after an item of 600 names, same thread");
}
