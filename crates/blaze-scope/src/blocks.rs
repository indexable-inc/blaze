//! A body's blocks that hold an `impl`, as the scope they are: the text of one unnamed module
//! per block, lowered by the one lowering and solved by the one name solver.
//!
//! The Reference, "Statements", "Item declarations": "Declaring an item within a statement
//! block restricts its scope to the block containing the statement." and, of what an inner
//! item sees, "There is no implicit capture of the containing function's generic parameters,
//! parameters, and local variables." So a block's item reads: the block's own items, then the
//! blocks around it, then the module: exactly a module whose declarations are the block's
//! item statements plus a glob import of the scope outside it (an explicit declaration shadows
//! a glob import: innermost first). An `impl` among the items is no name: it lands in the
//! crate's impl table like any impl, with the block's module as the module its header and
//! members are read in.
//!
//! Only a block on the way to an item that makes a module (`NestedKind::makes_module`) or to
//! a macro call in statement position is written (a body with neither makes nothing: a
//! block's `fn`, `const`, `static` and `use` are resolved by the body's own lowering), or to
//! a `fn`, `const` or `static` whose own body writes a block
//! ([`blaze_file_items::nested::NestedItem::body_writes`]: std's `init` declares `fn
//! reset_sigpipe` whose `#[cfg(..)]` block declares `mod sigpipe`; the fn is then an item of
//! its block's module, and its body's blocks are modules under that item's key, as any
//! module item's body's are: a `mod` in a block is a module like any other), with
//! EVERY item statement of it, in text order (a `macro_rules!` before its uses). A struct,
//! enum, union or alias of a block is so an item of the block's module: its head is made by
//! the ordinary code, in a scope without the enclosing function's generics and locals (the
//! quote above), and its derives expand there; [`block_modules`] gives a reader of the body
//! the module's key. A block's
//! module is named by its ordinal among the body's written blocks, in text order, so its key
//! and its impls' keys change only with an edit to the same body.
//!
//! A block's module is anonymous ([`is_block_module`]), as the Reference's block is ("Block
//! expressions": an anonymous scope, no module): a written `self` names the nearest module
//! around it that is no block's and `super` that module's parent (`paths.qualifiers.mod-self`,
//! `paths.qualifiers.super`; the cells' `Cells::up`, names.rs `written_up`), and a private,
//! `pub(self)` or `pub(super)` item of a block counts from that module (`vis.scoped`). Only the
//! `use super::*;` the render opens each block's module with reaches the block's own parent:
//! it is the block's lexical scope (the cells' `Cells::lexical_glob`).
//!
//! A macro call in statement position makes its block written: it is written into the
//! block's module and expanded there by the one expansion driver (it may declare items: the
//! Reference, "Macros", "Statements"). An expansion that is an expression writes nothing; one
//! that mixes items with `let` or expression statements, or is a block expression declaring
//! items (`tokio::select!`'s `mod __tokio_select_util`), is read as statements of the calling
//! block (crate::expanded_blocks `statements_as_items`, from resolve.rs `expand_decls`): its
//! top-level items are the calling block's, and each block inside it that holds such an item
//! or a statement call is a module under the call. The rendering is
//! [`crate::expanded_blocks::render`], one for hand-written and expansion-written blocks.

use core::fmt::Write as _;

use blaze_file_items::nested::Nested;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AString, AVec, Alloc};

/// The name of the module of a body's `ordinal`th written block.
pub const BLOCK_MODULE: &str = "__blaze_block_";

/// Whether a module named `name` is a block's module, written for a body's block
/// ([`BLOCK_MODULE`]) or for a block an expansion writes
/// ([`crate::expanded_blocks::EXPANSION_BLOCK`], which starts with it): an anonymous scope,
/// never the module a written `self`, `super`, `pub(self)` or `pub(super)` names. The name is
/// an input of the module's key, so this is a function of the key. The one test of it.
pub fn is_block_module(name: &str) -> bool {
    name.starts_with(BLOCK_MODULE)
}

/// Which blocks of `nested` are written as modules: one that holds an item that makes a
/// module ([`blaze_file_items::nested::NestedKind::makes_module`]) or a macro call in
/// statement position, and every block around it ([`crate::expanded_blocks::written`]).
fn written_blocks<'a>(nested: &Nested<'_>, alloc: Alloc<'a>) -> AVec<'a, bool> {
    crate::expanded_blocks::written(nested, alloc)
}

/// The key of the module of each block of `nested` (the scan of the whole text of the item or
/// member `body` of the crate `krate`), `None` for a block that is not written: the keys
/// `resolve.rs` `make_blocks` and the lowering give the modules of [`module_text`], derived
/// here from the same scan so a reader of the body (front2's `Locals::keep`) names a block's
/// module without a table: the fragment is `(body, "blocks", "", 0)`, a block's module
/// `(the fragment or the written block around it, "mod", its name, 0)`, its name
/// [`BLOCK_MODULE`] and its ordinal among the written blocks in text order (the order of
/// [`Nested::blocks`], which is the order their `{` is written).
pub fn block_modules<'a>(krate: CrateItemKey, body: ItemKey, nested: &Nested<'_>, alloc: Alloc<'a>) -> AVec<'a, Option<ItemKey>> {
    let written = written_blocks(nested, alloc);
    let fragment = crate::task::key_of(krate, body, "blocks", "", 0);
    let mut keys: AVec<'a, Option<ItemKey>> = AVec::new_in(alloc);
    keys.resize(nested.blocks.len(), None);
    let mut ordinal = 0u32;
    for (b, block) in nested.blocks.iter().enumerate() {
        if !written[b] {
            continue;
        }
        // A block's `{` is written after its parent's, so the parent's key is made; a written
        // block's parent is written.
        let outer = match block.parent {
            Some(p) => keys.get(p as usize).copied().flatten(),
            None => Some(fragment),
        };
        let mut name = AString::new_in(alloc);
        // Writing to an own string does not fail.
        let _ = write!(name, "{BLOCK_MODULE}{ordinal}");
        ordinal += 1;
        keys[b] = outer.map(|o| crate::task::key_of(krate, o, "mod", name.as_str(), 0));
    }
    keys
}

/// The module text of the blocks of `text` (one item's whole text, scanned to `nested`) that
/// hold an item that makes a module or a statement call, or a block that does; empty when no
/// block holds one. Rendered by [`crate::expanded_blocks::render`], the one renderer of
/// hand-written and expansion-written blocks. `copied` is told each slice of `text` the module
/// text copies (offsets in `text`): a body a macro wrote moves its marks' column with them
/// (crate_roots `remapped`); a file's body passes `&mut |_| {}`.
pub fn module_text<'a>(text: &str, nested: &Nested<'_>, copied: &mut dyn FnMut(crate::crate_roots::Copied), alloc: Alloc<'a>) -> AString<'a> {
    let written = written_blocks(nested, alloc);
    crate::expanded_blocks::render(text, nested, &written, crate::expanded_blocks::Outer::Module, copied, alloc)
}

#[cfg(test)]
mod tests {
    use super::module_text;
    use blaze_file_items::nested::scan;
    use blaze_own::Alloc;

    fn made(text: &str) -> std::string::String {
        let alloc = Alloc::global();
        module_text(text, &scan(text, alloc), &mut |_| {}, alloc).as_str().into()
    }

    #[test]
    fn a_block_with_an_impl_is_a_module() {
        let text = "const _: () = { use a::b as c; { fn only_here() {} } { impl A for X {} } struct L; impl c::B for X { fn f() { impl Inner for Y {} } } };";
        assert_eq!(
            made(text),
            "mod __blaze_block_0 {\nuse super::*;\nuse a::b as c;\nmod __blaze_block_1 {\nuse super::*;\nimpl A for X {}\n}\nstruct L;\nimpl c::B for X { fn f() { impl Inner for Y {} } }\n}\n"
        );
    }

    /// A statement call is written as the item-position call it also is (the Reference,
    /// "Macro invocation": `(..)` and `[..]` forms end with `;` as items), in text order with
    /// its attributes, and its block is written (the `if`'s block holding `d!();` is a module
    /// of its own: the call may declare items there, "Statements", "Item declarations": their
    /// scope is "the block containing the statement"); a call in an expression (`let x =
    /// v!();`) is written as one too: its expansion may be a block expression declaring items
    /// in its own block (tokio's `select!`), and the names library expands it only when its
    /// definition or input may write one (resolve.rs `expand_call`).
    #[test]
    fn a_statement_call_is_written_into_its_block() {
        let text = "fn f() { let x = v!(); #[cfg(a)] a::b!(X, (1)); impl A for X {} c! { impl B for X {} } if x { d!(); } e![x] }";
        assert_eq!(
            made(text),
            "mod __blaze_block_0 {\nuse super::*;\nv!();\n#[cfg(a)] a::b!(X, (1));\nimpl A for X {}\nc! { impl B for X {} }\nmod __blaze_block_1 {\nuse super::*;\nd!();\n}\ne![x];\n}\n"
        );
    }

    #[test]
    fn no_impl_no_module() {
        assert_eq!(made("fn f() { fn g() {} const K: u8 = 1; use a::b; let y = !m != 1; let s = \"impl struct\"; }"), "");
    }

    /// A body whose only statement is a macro call makes its block a module: what the call
    /// declares is known only once it is expanded (tokio's `select!` declares a `mod`).
    #[test]
    fn a_statement_call_alone_makes_a_module() {
        assert_eq!(made("fn f() -> u8 { fn g() {} m!() }"), "mod __blaze_block_0 {\nuse super::*;\nfn g() {}\nm!();\n}\n");
    }

    /// A block that declares a struct, an enum, a union or a type alias is a module too (the
    /// Reference, "Statements", "Item declarations": the item's "scope" is "the block
    /// containing the statement"), with its attributes, so a derive on it expands there; and
    /// `block_modules` names each written block's module as the lowering keys it: the
    /// fragment `(body, "blocks", "", 0)`, then `(outer, "mod", "__blaze_block_N", 0)`.
    #[test]
    fn a_block_with_a_type_is_a_module() {
        let text = "fn f() { if c { } { #[derive(Clone)] enum F { A, B } } struct V<'de> { m: P<&'de ()> } type Al = u8; let v = V { m: P }; }";
        assert_eq!(
            made(text),
            "mod __blaze_block_0 {\nuse super::*;\nmod __blaze_block_1 {\nuse super::*;\n#[derive(Clone)] enum F { A, B }\n}\nstruct V<'de> { m: P<&'de ()> }\ntype Al = u8;\n}\n"
        );
        let alloc = Alloc::global();
        let krate = blaze_id::CrateItemKey(blaze_id::ContentId::of("a test crate", b"a_block_with_a_type_is_a_module"));
        let body = crate::task::root_key(krate);
        let keys = super::block_modules(krate, body, &scan(text, alloc), alloc);
        let fragment = crate::task::key_of(krate, body, "blocks", "", 0);
        let outer = crate::task::key_of(krate, fragment, "mod", "__blaze_block_0", 0);
        let inner = crate::task::key_of(krate, outer, "mod", "__blaze_block_1", 0);
        let made: std::vec::Vec<_> = keys.iter().copied().flatten().collect();
        assert_eq!(made, [outer, inner]);
        // The `if`'s block and the struct literal's braces are not written.
        assert_eq!(keys.iter().filter(|k| k.is_none()).count(), keys.len() - 2);
    }

    /// A local fn whose own body declares a `mod` makes its block a module holding the fn
    /// (std's `init` > `reset_sigpipe` > `mod sigpipe`): the fn is that module's item, and its
    /// body is the names library's to write its own blocks for. Without
    /// `NestedItem::body_writes` in `written` nothing is written.
    #[test]
    fn a_local_fn_with_a_block_mod_is_a_module_item() {
        assert_eq!(
            made("fn init() { fn reset() -> u8 { { mod m { pub const A: u8 = 3; } m::A } } reset(); }"),
            "mod __blaze_block_0 {\nuse super::*;\nfn reset() -> u8 { { mod m { pub const A: u8 = 3; } m::A } }\n}\n"
        );
    }

    #[test]
    fn unrelated_braces_do_not_move_a_block() {
        // The ordinal counts written blocks only: an `if` block before changes no name.
        let a = made("fn f() { { impl A for X {} } }");
        let b = made("fn f() { if true { } { impl A for X {} } }");
        assert_eq!(a, b);
    }
}
