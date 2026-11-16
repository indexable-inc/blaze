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
//! Only a block on the way to an `impl`, a struct, an enum, a union or a type alias is
//! written (a body with none makes nothing: a block's `fn`, `const`, `static` and `use` are
//! resolved by the body's own lowering), with
//! EVERY item statement of it, in text order (a `macro_rules!` before its uses). A struct,
//! enum, union or alias of a block is so an item of the block's module: its head is made by
//! the ordinary code, in a scope without the enclosing function's generics and locals (the
//! quote above), and its derives expand there; [`block_modules`] gives a reader of the body
//! the module's key. A block's
//! module is named by its ordinal among the body's written blocks, in text order, so its key
//! and its impls' keys change only with an edit to the same body.
//!
//! Known differences from the Reference, each owed: `super::` written inside a nested item
//! names the item's module, not that module's parent; a private item of a block is private to
//! its block's module, where the language's privacy boundary is the enclosing module.
//!
//! A macro call in statement position of a written block is written into the block's module
//! too (it may write an impl), and expanded there by the one expansion driver; an expansion
//! that is an expression writes nothing (resolve.rs `expand_decls`). Owed: a statement call
//! in a body with NO written impl is not expanded (such a body makes no module: its call
//! could still write an impl), and an expansion mixing items with `let` or expression
//! statements is refused by name (`ExpandNote::NotItems`).

use core::fmt::Write as _;

use blaze_file_items::nested::{Nested, NestedKind};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AString, AVec, Alloc};

/// The name of the module of a body's `ordinal`th written block.
pub const BLOCK_MODULE: &str = "__blaze_block_";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Close,
    Open,
    Item(u32),
    Call(u32),
}

/// Which blocks of `nested` are written as modules: one that holds an item that makes a
/// module ([`NestedKind::makes_module`]: an impl, a struct, an enum, a union, a type alias),
/// and every block around it.
fn written_blocks<'a>(nested: &Nested<'_>, alloc: Alloc<'a>) -> AVec<'a, bool> {
    let blocks = &nested.blocks;
    let mut written: AVec<'a, bool> = AVec::new_in(alloc);
    written.resize(blocks.len(), false);
    for item in nested.items.iter().filter(|i| NestedKind::makes_module(i.kind)) {
        let mut at = Some(item.block);
        // A chain is at most as long as the body's blocks.
        for _ in 0..blocks.len() {
            let Some(b) = at else { break };
            if core::mem::replace(&mut written[b as usize], true) {
                break;
            }
            at = blocks[b as usize].parent;
        }
    }
    written
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
/// hold an impl, a struct, an enum, a union or a type alias, or a block that does; empty when
/// no block holds one.
pub fn module_text<'a>(text: &str, nested: &Nested<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    let blocks = &nested.blocks;
    let written = written_blocks(nested, alloc);
    let mut events: AVec<'a, (u32, Event)> = AVec::new_in(alloc);
    for (b, block) in blocks.iter().enumerate() {
        if written[b] {
            events.push((block.open, Event::Open));
            events.push((block.close, Event::Close));
        }
    }
    for (i, item) in nested.items.iter().enumerate() {
        if written[item.block as usize] {
            events.push((item.range.start, Event::Item(i as u32)));
        }
    }
    // A macro call in statement position of a written block: the Reference, "Macro
    // invocation", lists "Statements" among the places a macro is invoked, and an item
    // declaration is a statement, so the call may write items (an impl among them). It is
    // written into the block's module as the item-position call it also is, where the one
    // expansion driver expands it in text order; an expansion that does not read as items is
    // that driver's `ExpandNote::NotItems`, naming the macro.
    for (i, call) in nested.calls.iter().enumerate() {
        if written[call.block as usize] {
            events.push((call.range.start, Event::Call(i as u32)));
        }
    }
    // Text order: blocks nest and an item lies in its block, outside the blocks written
    // inside that block, so sorted events are well nested; a `}` before what starts at it.
    events.sort_unstable();
    let mut ordinal = 0u32;
    for (_, event) in events.iter() {
        match event {
            Event::Open => {
                // Writing to an own string does not fail.
                let _ = writeln!(out, "mod {BLOCK_MODULE}{ordinal} {{\nuse super::*;");
                ordinal += 1;
            }
            Event::Close => out.push_str("}\n"),
            Event::Item(i) => {
                let r = nested.items[*i as usize].range;
                out.push_str(&text[r.start as usize..r.end as usize]);
                out.push_str("\n");
            }
            Event::Call(i) => {
                let r = nested.calls[*i as usize].range;
                let call = &text[r.start as usize..r.end as usize];
                out.push_str(call);
                // The Reference, "Macro invocation": an item-position call with `(..)` or
                // `[..]` ends with `;`; one with `{..}` takes none.
                out.push_str(if call.ends_with('}') { "\n" } else { ";\n" });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::module_text;
    use blaze_file_items::nested::scan;
    use blaze_own::Alloc;

    fn made(text: &str) -> std::string::String {
        let alloc = Alloc::global();
        module_text(text, &scan(text, alloc), alloc).as_str().into()
    }

    #[test]
    fn a_block_with_an_impl_is_a_module() {
        let text = "const _: () = { use a::b as c; { fn only_here() {} } { impl A for X {} } struct L; impl c::B for X { fn f() { impl Inner for Y {} } } };";
        assert_eq!(
            made(text),
            "mod __blaze_block_0 {\nuse super::*;\nuse a::b as c;\nmod __blaze_block_1 {\nuse super::*;\nimpl A for X {}\n}\nstruct L;\nimpl c::B for X { fn f() { impl Inner for Y {} } }\n}\n"
        );
    }

    /// A statement call of a written block is written as the item-position call it also is
    /// (the Reference, "Macro invocation": `(..)` and `[..]` forms end with `;` as items), in
    /// text order with its attributes; a call in a block that is not written, or not in
    /// statement position, is not.
    #[test]
    fn a_statement_call_is_written_into_its_block() {
        let text = "fn f() { let x = v!(); #[cfg(a)] a::b!(X, (1)); impl A for X {} c! { impl B for X {} } if x { d!(); } e![x] }";
        assert_eq!(
            made(text),
            "mod __blaze_block_0 {\nuse super::*;\n#[cfg(a)] a::b!(X, (1));\nimpl A for X {}\nc! { impl B for X {} }\ne![x];\n}\n"
        );
    }

    #[test]
    fn no_impl_no_module() {
        assert_eq!(made("fn f() { fn g() {} const K: u8 = 1; use a::b; m!{ impl A for B {} } let s = \"impl struct\"; }"), "");
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

    #[test]
    fn unrelated_braces_do_not_move_a_block() {
        // The ordinal counts written blocks only: an `if` block before changes no name.
        let a = made("fn f() { { impl A for X {} } }");
        let b = made("fn f() { if true { } { impl A for X {} } }");
        assert_eq!(a, b);
    }
}
