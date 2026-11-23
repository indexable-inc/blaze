//! Items a macro expansion declares inside a body, through the one-call entry: the blocks of a
//! body are found from its text AFTER its statement calls are expanded
//! ([`crate::expanded_blocks`]).
//!
//! The rules, from the Reference:
//! - "Block expressions": a block is an anonymous namespace scope for the items in it;
//!   "Statements", "Item declarations": "Declaring an item within a statement block restricts
//!   its scope to the block containing the statement."
//! - "Macros": "Macros can be invoked in the following situations: ... Statements"; an
//!   expansion's tokens may declare items.
//! - "Names", "Scopes", item scopes: "The name of an item declared directly in a block is in
//!   scope for the entire block", so before its declaration too.
//! - "Macros by example", "Hygiene": "`macro_rules` macros have mixed-site hygiene. This means
//!   that loop labels, block labels, and local variables are looked up at the macro definition
//!   site while other symbols are looked up at the macro invocation site." An item is none of
//!   those: its scope is the block it is declared in, not its span. So a name the expansion's
//!   block declares is no name of the block around the call.
//!
//! The shape is tokio 1.53.3 macros/select.rs's: an expansion that is ONE block expression
//! declaring a `mod` and naming through it. Needs `NestedKind::Mod` to make a block a module
//! (wiring census14-B-1, edit 1).

use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_surface::{DefKind, Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::Written;
use crate::resolve::Names;

const MACRO_BLOCKS: &str = "#![no_core]
macro_rules! m { () => {{ mod inner { pub const K: u8 = 7; } use inner::K; K }} }
macro_rules! later { () => {{ use inner::K; mod inner { pub const K: u8 = 8; } K }} }
pub fn f() -> u8 { m!() }
pub fn g() -> u8 { later!() }
";

static MACRO_BLOCKS_TEXTS: Texts = Texts(&[&[("/user/lib.rs", MACRO_BLOCKS)]]);

/// The module of the body's outermost block, by the same scan the names library makes
/// (`blocks::block_modules`): the body `name` is the text from `pub fn {name}` to `until`.
fn calling_block(names: &Names<'static>, name: &str, until: &str) -> ItemKey {
    let alloc = blaze_own::testing::heap().alloc();
    let root = names.root(0);
    let item = names
        .item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, alloc)
        .unwrap_or_else(|e| panic!("`{name}`: {e}"));
    let head = std::format!("pub fn {name}");
    let from = MACRO_BLOCKS.find(head.as_str()).unwrap_or_else(|| panic!("the text holds `{head}`"));
    let to = MACRO_BLOCKS[from..].find(until).map_or(MACRO_BLOCKS.len(), |i| from + i);
    let nested = blaze_file_items::nested::scan(&MACRO_BLOCKS[from..to], alloc);
    let modules = crate::blocks::block_modules(names.krate(0).key, item.def.key, &nested, alloc);
    modules.first().copied().flatten().unwrap_or_else(|| panic!("`{name}`'s block holds a statement call: it is written as a module"))
}

/// The one module the expansion of the call in `calling` made: a child of `calling` named
/// `crate::expanded_blocks::EXPANSION_BLOCK`, the call's tag and `_0`.
fn expansion_block(names: &Names<'static>, calling: ItemKey) -> ItemKey {
    let made: Vec<ItemKey> = names
        .krate(0)
        .made
        .iter()
        .filter(|(_, m)| m.info.parent == Some(calling) && m.info.name.is_some_and(|n| n.starts_with(crate::expanded_blocks::EXPANSION_BLOCK)))
        .map(|(k, _)| *k)
        .collect();
    assert_eq!(made.len(), 1, "the expansion's one block is one module under the calling block");
    made[0]
}

/// `fn f() -> u8 { m!() }`, `m!()` expanding to `{ mod inner { pub const K: u8 = 7; } use
/// inner::K; K }`: the expansion's block is a module holding `inner` and the `use`, and `K`
/// read there is the const. Before `crate::expanded_blocks`, the body has no hand-written
/// item, makes no block module, and the expansion is never read by the names library: the
/// test fails at "`f`'s block holds a statement call".
/// Prints: "MACRO BLOCK: K resolves in the expansion's block".
#[test]
fn an_expansions_block_declares_a_mod_and_a_use() {
    let alloc = blaze_own::testing::heap().alloc();
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &MACRO_BLOCKS_TEXTS);
    let calling = calling_block(&names, "f", "pub fn g");
    let block = expansion_block(&names, calling);
    let inner = names
        .item_at(block, Namespace::Types, Written { kind: PathKind::Plain, segments: &["inner"] }, alloc)
        .unwrap_or_else(|e| panic!("`inner` in the expansion's block: {e}"));
    assert_eq!(inner.def.kind, DefKind::Module, "`inner` is the block's module");
    let k = names
        .item_at(block, Namespace::Values, Written { kind: PathKind::Plain, segments: &["K"] }, alloc)
        .unwrap_or_else(|e| panic!("`K` in the expansion's block: {e}"));
    let through = names
        .item_at(block, Namespace::Values, Written { kind: PathKind::Plain, segments: &["inner", "K"] }, alloc)
        .unwrap_or_else(|e| panic!("`inner::K` in the expansion's block: {e}"));
    assert_eq!(k.def.key, through.def.key, "the `use` binds the const `inner` declares");
    std::println!("MACRO BLOCK: K resolves in the expansion's block");
}

/// `later!()` writes `use inner::K;` BEFORE `mod inner { .. }`: the Reference's "in scope for
/// the entire block" makes the `use` read the `mod` declared after it.
/// Prints: "MACRO BLOCK: an item declared after its use resolves".
#[test]
fn an_item_declared_after_its_use_resolves() {
    let alloc = blaze_own::testing::heap().alloc();
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &MACRO_BLOCKS_TEXTS);
    let calling = calling_block(&names, "g", "\n");
    let block = expansion_block(&names, calling);
    let k = names
        .item_at(block, Namespace::Values, Written { kind: PathKind::Plain, segments: &["K"] }, alloc)
        .unwrap_or_else(|e| panic!("`K`, imported before its module is declared: {e}"));
    let through = names
        .item_at(block, Namespace::Values, Written { kind: PathKind::Plain, segments: &["inner", "K"] }, alloc)
        .unwrap_or_else(|e| panic!("`inner::K` in the expansion's block: {e}"));
    assert_eq!(k.def.key, through.def.key);
    std::println!("MACRO BLOCK: an item declared after its use resolves");
}

/// The names the expansion's block declares are no names of the block around the call nor of
/// the crate's module: hygiene of items is by scope (the Reference's "Hygiene" quoted above
/// sends an item to the invocation site, and "Item declarations" ends its scope at its block).
/// A fix that put the expansion's items into the calling block (or the module) fails here.
/// Prints: "MACRO BLOCK: nothing leaks out of the expansion's block".
#[test]
fn an_expansions_names_do_not_leak_out_of_its_block() {
    let alloc = blaze_own::testing::heap().alloc();
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &MACRO_BLOCKS_TEXTS);
    let calling = calling_block(&names, "f", "pub fn g");
    let root = names.root(0);
    for (at, what) in [(calling, "the calling block"), (root, "the crate's module")] {
        for (ns, name) in [(Namespace::Values, "K"), (Namespace::Types, "inner")] {
            assert!(
                names.item_at(at, ns, Written { kind: PathKind::Plain, segments: &[name] }, alloc).is_err(),
                "`{name}` of the expansion's block is no name of {what}"
            );
        }
    }
    std::println!("MACRO BLOCK: nothing leaks out of the expansion's block");
}

/// An attribute on an item inside a block replaces that item with items in the
/// same block. Its sibling module shadows an enclosing module for the whole block.
#[test]
fn a_local_attribute_writes_siblings_in_its_own_block() {
    use crate::names::{World, path};
    use crate::names_record::{NamesRecord, Verified, encode};
    use blaze_own::Alloc;

    const LIB: &str = "#![no_core]
#[macro_export]
macro_rules! add_module {
    attr() (pub struct $name:ident { $field:ident: $ty:ty }) => {
        pub mod slots { use super::*; pub const FIELD_A: u8 = 1; }
        pub struct $name { pub $field: $ty }
    };
}
mod slots { pub const FIELD_A: bool = false; }
fn f() {
    let outer = slots::FIELD_A;
    {
        let inner = slots::FIELD_A;
        #[crate::add_module] pub struct Layout { field_a: u8 }
    }
}";
    static FILES: Texts = Texts(&[&[("/user/lib.rs", LIB)]]);
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &FILES);
    let alloc = blaze_own::testing::heap().alloc();
    let root = names.root(0);
    let f = names.item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments: &["f"] }, alloc).expect("f").def.key;
    let body = names.item_text(f).expect("f's source");
    let nested = blaze_file_items::nested::scan(body.text, alloc);
    let modules = crate::blocks::block_modules(crates[0].key, f, &nested, alloc);
    let [Some(outer), Some(inner)] = modules.as_slice() else { panic!("the blocks around Layout are modules: {modules:?}") };

    fn constant(world: &impl World<&'static str>, module: ItemKey, alloc: Alloc<'_>) -> ItemKey {
        path(world, module, Namespace::Values, Written { kind: PathKind::Plain, segments: &["slots", "FIELD_A"] }, alloc)
            .expect("slots::FIELD_A resolves").whole(2).expect("slots::FIELD_A names a value").key
    }
    let enclosing = constant(&names, root, alloc);
    let sibling = constant(&names, *inner, alloc);
    assert_ne!(sibling, enclosing, "reading the enclosing module would select its bool instead");
    assert_eq!(constant(&names, *outer, alloc), enclosing, "the generated module is private to the inner block");
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bytes = encode(names.krate(0), alloc).expect("encode both block scopes");
    let records = NamesRecord::open(Verified::check(crate::resolve::aligned(&bytes, erased.bump())).expect("reopen both block scopes"));
    assert_eq!(constant(&records, *inner, alloc), sibling);
    assert_eq!(constant(&records, *outer, alloc), enclosing);
    std::println!("slots::FIELD_A resolves to the attribute's sibling only inside its block, live and reopened");
}

/// An item-position expansion writes an anonymous const containing an impl and a
/// sibling macro invocation. A leading `::` on the invocation still declares its
/// output in the containing block. Both methods must see the block's own item.
#[test]
fn an_absolute_statement_call_declares_a_sibling_of_generated_methods() {
    const USER: &str = "#![no_core]
struct S;
const COUNT: bool = false;
macro_rules! generate { () => {
    const _: () = {
        impl S {
            fn first() -> u8 { COUNT }
            fn second() -> u8 { COUNT }
        }
        ::dep::declare!(COUNT);
    };
} }
generate!();
";
    const DEP: &str = "#![no_core]
#[macro_export]
macro_rules! declare { ($name:ident) => { const $name: u8 = 1; }; }
";
    static FILES: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &FILES);
    let [imp] = names.krate(0).tables.impls.as_slice() else { panic!("the expansion declares one impl") };
    let members = names.members(imp.key).expect("the impl declares two methods");
    assert_eq!(members.iter().map(|m| m.name).collect::<Vec<_>>(), [Some("first"), Some("second")]);
    let alloc = blaze_own::testing::heap().alloc();
    let binding = |module, name| {
        names.item_at(module, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, alloc)
            .unwrap_or_else(|e| panic!("{name} in {module:?}: {e}")).def.key
    };
    // Dropping the macro invocation still leaves a visible COUNT via the lexical
    // glob. Its owner distinguishes that false positive from the sibling item.
    let outer = binding(names.root(0), "COUNT");
    for member in members {
        let (_, body) = member.text.split_once('{').expect("a method body");
        let name = body.trim().strip_suffix('}').expect("a closed method body").trim();
        assert_eq!(name, "COUNT");
        let sibling = binding(imp.module, name);
        assert_ne!(sibling, outer);
        let declaration = names.item_text(sibling).expect("the sibling declaration");
        assert_eq!(declaration.module, imp.module);
        let text = declaration.text.split_whitespace().collect::<std::string::String>();
        assert_eq!(text.trim_end_matches(';'), "constCOUNT:u8=1");
    }
    std::println!("both methods see the sibling COUNT in their generated block");
}

/// The Reference, "Macros by example", "Hygiene": `$crate` names the crate defining the macro.
/// Both roots bind `T`, so a lost mark gives a successful lookup of the wrong definition.
#[test]
fn dollar_crate_in_a_copied_field_names_the_defining_crate() {
    use crate::crate_roots::{RootsLost, TextMarks, column_in};
    use crate::expansion_origins::OriginsError;
    use crate::expansion_records::{self, Directory, ExpansionRecord};
    use crate::items_record::{self, ItemsRecord, TextOf};
    use crate::names::{World, path};
    use crate::names_record::{self, LoadNames, RecordNames};
    use crate::resolve::{At, aligned};
    use blaze_id::CrateItemKey;
    use blaze_own::Alloc;

    const USER: &str = "#![no_core]\nstruct T; dep::make!();";
    const DEP: &str = "#![no_core]\npub struct T;
#[macro_export] macro_rules! make { () => { fn f() { struct L($crate::T); } }; }";
    static FILES: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);

    fn field_type<'t>(
        world: &impl World<&'t str>, caller: CrateItemKey, module: ItemKey, item: &'t str,
        marks: Option<TextMarks<'_>>, alloc: Alloc<'_>,
    ) -> ItemKey {
        let (_, tuple) = item.split_once('(').expect("L's tuple fields");
        let (ty, _) = tuple.split_once(')').expect("L's one field type");
        let mark = |word| marks.and_then(|m| m.mark_of(word).expect("field token in the block text"));
        let ty = crate::task::parse_path_marked(ty, caller, &mark, alloc).expect("a field type path");
        let written = Written { kind: ty.kind, segments: &ty.segments };
        path(world, module, Namespace::Types, written, alloc)
            .expect("resolve the field type")
            .whole(ty.segments.len())
            .expect("the complete type path")
            .key
    }

    struct Loaded<'a>([&'a [u8]; 2]);
    impl<'a> LoadNames<'a> for Loaded<'a> {
        fn load(&self, krate: usize) -> Option<&'a [u8]> {
            self.0.get(krate).copied()
        }
    }

    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &FILES);
    let alloc = blaze_own::testing::heap().alloc();
    let user = names.krate(0);
    let t = |i| names.item_at(names.root(i), Namespace::Types, Written { kind: PathKind::Plain, segments: &["T"] }, alloc).expect("root T").def.key;
    let (dep_t, caller_t) = (t(1), t(0));
    assert_ne!(dep_t, caller_t);
    let f = names.item_at(names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["f"] }, alloc).expect("macro-written f");
    let block = crate::task::key_of(user.key, f.def.key, "blocks", "", 0);
    let local = user.tables.texts.values().map(|(item, _)| item).find(|item| item.name == Some("L")).expect("local struct L");
    assert_eq!(names.place(local.key).expect("L's place").at, At::Expansion(block));
    let made = user.expansions[&block].made.expect("f's block module");
    let lookup = |column| {
        let marks = match column {
            Ok(column) => Some(TextMarks { text: made.text, column }),
            Err(RootsLost::NoOrigins { error: OriginsError::Rewritten { .. } }) => None,
            Err(error) => panic!("L's block origins: {error}"),
        };
        field_type(&names, user.key, local.module, local.text, marks, alloc)
    };
    assert_eq!(lookup(made.column), dep_t);
    let planted = Err(OriginsError::Rewritten { what: "a block's module text" });
    assert_eq!(lookup(column_in(&planted)), caller_t);

    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let keys = crates.each_ref().map(|c| c.key);
    let loaded = Loaded([0, 1].map(|i| aligned(&names_record::encode(names.krate(i), alloc).expect("encode names"), bump)));
    let records = RecordNames::new(&keys, loaded, alloc);
    let split = expansion_records::write(user, alloc).expect("encode expansions");
    let directory = Directory::open(&split.directory).expect("open expansion directory");
    let items = ItemsRecord::open(aligned(&items_record::encode(user, alloc).expect("encode items"), bump), alloc).expect("open items");
    let row = items.item(local.key).expect("read item row").expect("recorded L");
    let span = row.text.expect("L's recorded text span");
    assert_eq!(span.at, At::Expansion(block));
    let entry = directory.entry(block).expect("read directory row").expect("recorded block module");
    let At::Expansion(writer) = names.place(f.def.key).expect("f's place").at else { panic!("make! wrote f") };
    let writer = directory.entry(writer).expect("read writer's row").expect("recorded make! invocation");
    assert_eq!(entry.call_part, writer.call_part, "copied tokens refer to the original invocation");
    let bytes = &split.records.iter().find(|r| r.key == entry.record).expect("the block's record").bytes;
    let held = ExpansionRecord::open(bytes).expect("open the block's record");
    let text = entry.text(&held).expect("the block's text");
    let item = items.slice(span, &|at| (at == TextOf::Expansion { call: block }).then_some(text)).expect("L's declaration");
    let origins = entry.origins(&held).expect("read block origins").expect("the block keeps origins");
    let marks = TextMarks { text, column: origins.column };
    assert_eq!(field_type(&records, user.key, row.module, item, Some(marks), alloc), dep_t);
    std::println!("MACRO BLOCK: L's field resolves to dep::T over names and records; without marks it resolves to user::T");
}
