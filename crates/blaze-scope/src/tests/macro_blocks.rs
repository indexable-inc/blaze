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
