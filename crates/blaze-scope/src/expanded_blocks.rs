//! The blocks of a body, by hand or by a macro expansion, as the modules the names library
//! makes of them: one mechanism for both.
//!
//! The Reference, "Block expressions": "a block is ... an anonymous namespace scope" for the
//! items declared in it; "Statements", "Item declarations": "Declaring an item within a
//! statement block restricts its scope to the block containing the statement." "Macros":
//! "Macros can be invoked in the following situations: ... Statements", and a statement
//! expansion is statements, items among them. "Names", "Scopes", item scopes: "The name of an
//! item declared directly in a block is in scope for the entire block", before its
//! declaration too. And, "Macros by example", "Hygiene": "`macro_rules` macros have mixed-site
//! hygiene. This means that loop labels, block labels, and local variables are looked up at
//! the macro definition site while other symbols are looked up at the macro invocation site."
//! So an item an expansion declares is scoped by the block it stands in, never by its span:
//! `tokio::select!` writes `{ mod __tokio_select_util { .. } let mut disabled:
//! __tokio_select_util::Mask = ..; .. }`, and `__tokio_select_util` is a name of that block.
//!
//! What is here:
//! - [`written`]: which blocks of a scan are written as modules: one on the way to an item
//!   that makes a module ([`NestedKind::makes_module`]) or to a macro call in statement
//!   position (its expansion may declare items in the block: what it writes is known only
//!   after it is expanded, by the one expansion driver), and every block around it.
//! - [`render`]: the module text of the written blocks (`crate::blocks::module_text` is it
//!   with [`Outer::Module`]).
//! - [`statements_as_items`]: an expansion of a statement call that does not read as items
//!   (it mixes items with `let` and expression statements, or is a block expression) read as
//!   the statements of the calling block: its item statements and statement calls at its top
//!   level are the calling block's, and each block inside it that [`written`] keeps is a
//!   module of its own, named under the call ([`EXPANSION_BLOCK`] and the call's tag), written
//!   into the call's fragment where the lowering makes it like any `mod` an expansion writes.
//!   A `let` or an expression statement binds no item: nothing of it is written.
//!
//! A call inside such a block is written into its module and expanded there by the same
//! driver: an expansion in an expansion is the same step again, driven by the solve's work
//! list, as deep as the driver's own depth bound allows. No recursion here.

use core::fmt::Write as _;

use blaze_file_items::nested::{Nested, NestedEnd, scan};
use blaze_id::ItemKey;
use blaze_own::{AString, AVec, Alloc};

use crate::blocks::BLOCK_MODULE;

/// The prefix of the name of a module of a block an expansion writes: `__blaze_block_x`, the
/// call's tag ([`tag`], 16 hex digits) and the block's ordinal among the expansion's written
/// blocks in text order, joined by `_`. It starts with [`BLOCK_MODULE`] (a call in it is a
/// statement call, as in any block's module) and never equals a hand block's name (those are
/// [`BLOCK_MODULE`] and digits); the tag keeps two calls of one block apart.
pub const EXPANSION_BLOCK: &str = "__blaze_block_x";

/// Why an expansion's statements were not read, naming the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementsRefused {
    /// The expansion's text closes a bracket it did not open or leaves one open: its blocks
    /// cannot be told apart (the scan's [`NestedEnd::Unbalanced`]).
    Unbalanced,
}

impl StatementsRefused {
    /// The refusal's words, for the driver's note.
    pub fn message(self) -> &'static str {
        match self {
            StatementsRefused::Unbalanced => {
                "a statement call's expansion whose brackets do not balance: its blocks (the scope of the items it declares) cannot be read"
            }
        }
    }
}

impl core::fmt::Display for StatementsRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

/// Where the outermost block of a rendered text is.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Outer {
    /// A body's own text: every written block is a module, named [`BLOCK_MODULE`] and its
    /// ordinal among the body's written blocks.
    Module,
    /// An expansion of a statement call, wrapped in one `{ .. }` that stands for the calling
    /// block: that block is not written as a module (its items are the calling block's); each
    /// written block inside it is, named [`EXPANSION_BLOCK`], the call's [`tag`] and its
    /// ordinal.
    Inline { call: ItemKey },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Close,
    /// The written block at this index of [`Nested::blocks`].
    Open(u32),
    Item(u32),
    Call(u32),
}

/// The tag of a call in the names of its expansion's block modules: the first eight bytes of
/// its key, little-endian (the key is item-relative: derived from the body's key and the
/// call's place among the body's calls, never a text offset of the file).
pub fn tag(call: ItemKey) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&call.0.0[..8]);
    u64::from_le_bytes(b)
}

/// The name of the `ordinal`th written block of the expansion of `call`.
pub fn expansion_block_name<'a>(call: ItemKey, ordinal: u32, alloc: Alloc<'a>) -> AString<'a> {
    let mut name = AString::new_in(alloc);
    // Writing to an own string does not fail.
    let _ = write!(name, "{EXPANSION_BLOCK}{:016x}_{ordinal}", tag(call));
    name
}

/// Which blocks of `nested` are written as modules: one that holds an item that writes it
/// ([`blaze_file_items::nested::NestedItem::writes`]: an item that makes a module, or a `fn`,
/// `const` or `static` whose own body writes a block, so that item is the block module's and
/// its body's blocks are modules under it) or a macro call in statement position, and every
/// block around it.
pub(crate) fn written<'a>(nested: &Nested<'_>, alloc: Alloc<'a>) -> AVec<'a, bool> {
    let blocks = &nested.blocks;
    let mut written: AVec<'a, bool> = AVec::new_in(alloc);
    written.resize(blocks.len(), false);
    let items = nested.items.iter().filter(|i| i.writes()).map(|i| i.block);
    let calls = nested.calls.iter().map(|c| c.block);
    for start in items.chain(calls) {
        let mut at = Some(start);
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

/// The module text of the blocks of `text` (scanned to `nested`) that `written` keeps: for
/// each, a `mod` holding FIRST `use super::*;` (an explicit declaration shadows a glob import:
/// innermost first). That glob is the block's lexical scope. The cells know it by its place,
/// the first import of the module's own text, and resolve it to the module's own parent, where
/// a written `super` passes over blocks (`Cells::lexical_glob`). Then comes EVERY item
/// statement and statement call of the block, in text order
/// (a `macro_rules!` before its uses). With [`Outer::Inline`] the outermost block (block 0,
/// the calling block) has no `mod` line: its items and calls are written at the top level.
///
/// `copied` is told each slice of `text` written into the result (token-mark-2: the origin
/// column moves with them, crate::crate_roots::remap).
pub(crate) fn render<'a>(
    text: &str,
    nested: &Nested<'_>,
    written: &[bool],
    outer: Outer,
    copied: &mut dyn FnMut(crate::crate_roots::Copied),
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut copy = |out: &AString<'_>, from: u32, len: u32| {
        copied(crate::crate_roots::Copied { from, to: u32::try_from(out.len()).unwrap_or(u32::MAX), len });
    };
    let mut out = AString::new_in(alloc);
    let is_written = |b: u32| written.get(b as usize).copied().unwrap_or(false);
    let mut events: AVec<'a, (u32, Event)> = AVec::new_in(alloc);
    for (b, block) in nested.blocks.iter().enumerate() {
        let inlined = matches!(outer, Outer::Inline { .. }) && b == 0;
        if is_written(b as u32) && !inlined {
            events.push((block.open, Event::Open(b as u32)));
            events.push((block.close, Event::Close));
        }
    }
    for (i, item) in nested.items.iter().enumerate() {
        if is_written(item.block) {
            events.push((item.range.start, Event::Item(i as u32)));
        }
    }
    // A macro call in statement position of a written block: the Reference, "Macro
    // invocation", lists "Statements" among the places a macro is invoked, and an item
    // declaration is a statement, so the call may write items. It is written into the block's
    // module as the item-position call it also is, where the one expansion driver expands it
    // in text order.
    for (i, call) in nested.calls.iter().enumerate() {
        if is_written(call.block) {
            events.push((call.range.start, Event::Call(i as u32)));
        }
    }
    // Text order: blocks nest and an item lies in its block, outside the blocks written
    // inside that block, so sorted events are well nested; a `}` before what starts at it.
    events.sort_unstable();
    let mut ordinal = 0u32;
    for (_, event) in events.iter() {
        match *event {
            Event::Open(b) => {
                // The block's outer attributes go on its module: a false `cfg` removes the
                // block and so the module, its items and its calls with it (hyper's `trace!`:
                // `#[cfg(feature = "tracing")] { tracing::trace!(..); }`). The Reference,
                // "Conditional compilation": a false `cfg` removes the thing it is on.
                let block = nested.blocks[b as usize];
                let attrs = &text[block.attrs.start as usize..block.attrs.end as usize];
                // Outer ones only (`#[..]`, `# [..]` in an expansion's printed tokens): an
                // inner `#![..]` at the start of the block around is that block's own.
                let outer_attr = attrs.strip_prefix('#').is_some_and(|rest| rest.trim_start().starts_with('['));
                if outer_attr {
                    copy(&out, block.attrs.start, block.attrs.end - block.attrs.start);
                    out.push_str(attrs);
                    out.push_str("\n");
                }
                // Writing to an own string does not fail.
                let _ = match outer {
                    Outer::Module => writeln!(out, "mod {BLOCK_MODULE}{ordinal} {{\nuse super::*;"),
                    Outer::Inline { call } => writeln!(out, "mod {EXPANSION_BLOCK}{:016x}_{ordinal} {{\nuse super::*;", tag(call)),
                };
                ordinal += 1;
            }
            Event::Close => out.push_str("}\n"),
            Event::Item(i) => {
                let r = nested.items[i as usize].range;
                copy(&out, r.start, r.end - r.start);
                out.push_str(&text[r.start as usize..r.end as usize]);
                out.push_str("\n");
            }
            Event::Call(i) => {
                let c = nested.calls[i as usize];
                // An expression call goes under its statement's outer attributes: a false
                // `cfg` removes it as it removes the statement (the Reference, "Conditional
                // compilation"; tempo actor.rs:325, `#[cfg(feature = "fuzz")] let details =
                // serde_json::json!(..);`, whose crate is no name without the feature). A
                // statement call's path holds its own.
                if c.attrs.end > c.attrs.start {
                    copy(&out, c.attrs.start, c.attrs.end - c.attrs.start);
                    out.push_str(&text[c.attrs.start as usize..c.attrs.end as usize]);
                    out.push_str("\n");
                }
                let r = c.range;
                let call = &text[r.start as usize..r.end as usize];
                copy(&out, r.start, r.end - r.start);
                out.push_str(call);
                // The Reference, "Macro invocation": an item-position call with `(..)` or
                // `[..]` ends with `;`; one with `{..}` takes none.
                out.push_str(if call.ends_with('}') { "\n" } else { ";\n" });
            }
        }
    }
    out
}

/// Where an expansion's own text starts in the text it is scanned as ([`wrapped`]): after the
/// one `{` that stands for the calling block. An offset of the expansion's text is this much
/// more in the scan.
pub const WRAP_OPEN: u32 = 1;

/// The text a statement call's expansion `text` is scanned as. The Reference, "Macro
/// invocation", "Statements": the expansion is the statements of the calling block, so one
/// `{ .. }` around it stands for that block (block 0 of the scan).
fn wrapped<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut wrapped = AString::new_in(alloc);
    wrapped.push_str("{");
    wrapped.push_str(text);
    wrapped.push_str("\n}");
    wrapped
}

/// Each block's ordinal among an expansion's written blocks, as [`render`] names them with
/// [`Outer::Inline`]: block 0 (the calling block) is inlined and has none, and every other
/// written block takes the next number in the order of its `{` (the scan pushes blocks in
/// that order, and `render` opens them sorted by offset). `None` for a block not written.
fn ordinals<'a>(written: &[bool], alloc: Alloc<'a>) -> AVec<'a, Option<u32>> {
    let mut out: AVec<'a, Option<u32>> = AVec::with_capacity_in(written.len(), alloc);
    let mut next = 0u32;
    for (b, w) in written.iter().enumerate() {
        if b > 0 && *w {
            out.push(Some(next));
            next += 1;
        } else {
            out.push(None);
        }
    }
    out
}

/// The blocks of a statement call's expansion as the names library files them: the scan of
/// the wrapped text (block 0 stands for the calling block; an offset of the expansion's text
/// is [`WRAP_OPEN`] more there) and each block's ordinal among the written blocks
/// ([`expansion_block_name`] takes it). A reader that keys a block of the expansion it lowered
/// (blaze-ladder front2 scope_table.rs) numbers it here, so both layers agree by construction.
pub struct ExpansionBlocks<'a> {
    pub nested: Nested<'a>,
    pub ordinals: AVec<'a, Option<u32>>,
}

/// [`ExpansionBlocks`] of the expansion text `text`.
pub fn expansion_blocks<'a>(text: &str, alloc: Alloc<'a>) -> Result<ExpansionBlocks<'a>, StatementsRefused> {
    let wrapped = wrapped(text, alloc);
    let nested = scan(wrapped.as_str(), alloc);
    if nested.end != NestedEnd::Whole {
        return Err(StatementsRefused::Unbalanced);
    }
    let kept = written(&nested, alloc);
    let ordinals = ordinals(&kept, alloc);
    Ok(ExpansionBlocks { nested, ordinals })
}

/// The expansion `text` of the statement call `call`, which does not read as items, read as
/// statements of the calling block: the item text the driver lowers as the call's fragment
/// (its top-level items and calls, and a `mod` per written block inside it), `None` when it
/// declares nothing and calls nothing in statement position (`assert!(..)`, `println!(..)`:
/// nothing of it is a name).
///
/// With the slices of `text` it copied (offsets in `text`, not in the wrapped scan text).
#[allow(clippy::type_complexity)]
pub(crate) fn statements_as_items<'a>(text: &str, call: ItemKey, alloc: Alloc<'a>) -> Result<Option<(AString<'a>, AVec<'a, crate::crate_roots::Copied>)>, StatementsRefused> {
    // The calling block in the scan ([`wrapped`], where the rule is quoted).
    let wrapped = wrapped(text, alloc);
    let nested = scan(wrapped.as_str(), alloc);
    if nested.end != NestedEnd::Whole {
        return Err(StatementsRefused::Unbalanced);
    }
    if nested.items.is_empty() && nested.calls.is_empty() {
        return Ok(None);
    }
    let mut kept = written(&nested, alloc);
    // Every item statement of the calling block is its, whatever its kind.
    if let Some(first) = kept.first_mut() {
        *first = true;
    }
    // The scan text is `{` + `text` + `\n}`: a slice of it at `from` is `text`'s at `from - 1`;
    // only slices inside `text` are copied (items, calls and attributes of blocks within it).
    let mut copies: AVec<'a, crate::crate_roots::Copied> = AVec::new_in(alloc);
    let out = render(
        wrapped.as_str(),
        &nested,
        &kept,
        Outer::Inline { call },
        &mut |c| copies.push(crate::crate_roots::Copied { from: c.from.saturating_sub(WRAP_OPEN), ..c }),
        alloc,
    );
    Ok((!out.is_empty()).then_some((out, copies)))
}

#[cfg(test)]
mod tests {
    use super::{EXPANSION_BLOCK, StatementsRefused, statements_as_items, tag};
    use blaze_id::{ContentId, ItemKey};

    fn call() -> ItemKey {
        ItemKey(ContentId::of("a test call", b"expanded_blocks"))
    }

    /// tokio 1.53.3 macros/select.rs writes `mod __tokio_select_util { .. }` inside the block
    /// it expands to, then names `__tokio_select_util::Mask` in that block: the block is a
    /// module under the call (the Reference, "Statements": the item's scope is "the block
    /// containing the statement"), holding the `mod`; the `let` and the `loop` write nothing.
    /// Needs `NestedKind::Mod` to make a module (wiring census14-B-1, edit 1). Prints nothing.
    #[test]
    fn a_select_shaped_block_is_a_module_under_its_call() {
        let alloc = blaze_own::testing::heap().alloc();
        let text = "{ mod __tokio_select_util { pub(super) type Mask = u8; } let mut disabled: __tokio_select_util::Mask = 0; loop { if disabled == 0 { break; } } }";
        let got = statements_as_items(text, call(), alloc).unwrap_or_else(|e| panic!("read: {e}"));
        let want = std::format!(
            "mod {EXPANSION_BLOCK}{:016x}_0 {{\nuse super::*;\nmod __tokio_select_util {{ pub(super) type Mask = u8; }}\n}}\n",
            tag(call())
        );
        assert_eq!(got.as_ref().map(|(s, _)| s.as_str()), Some(want.as_str()));
    }

    /// A statement expansion's top-level items and statement calls are the calling block's
    /// (no `mod` line around them); its `let` is no item. Prints nothing.
    #[test]
    fn top_level_items_are_the_calling_blocks() {
        let alloc = blaze_own::testing::heap().alloc();
        let got = statements_as_items("struct S; let x = S; inner!(x);", call(), alloc).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(got.as_ref().map(|(s, _)| s.as_str()), Some("struct S;\ninner!(x);\n"));
    }

    /// The ordinals [`super::expansion_blocks`] gives are the numbers in the names `render`
    /// writes, a written block inside a written block included (`_1` inside `_0`), and the
    /// calling block and a block that declares nothing have none. A numbering that counted
    /// the unwritten `{ let y = 1; }` or the calling block would give `_2` to the last block and
    /// fail. Prints nothing.
    #[test]
    fn ordinals_are_the_rendered_names() {
        let alloc = blaze_own::testing::heap().alloc();
        let text = "{ mod a { } { struct S; } } { let y = 1; } { struct T; }";
        let blocks = super::expansion_blocks(text, alloc).unwrap_or_else(|e| panic!("scan: {e}"));
        let got: std::vec::Vec<Option<u32>> = blocks.ordinals.iter().copied().collect();
        // Blocks: 0 the calling block, 1 `{ mod a .. }`, 2 `{ struct S; }` inside it, 3 `{ let
        // y = 1; }`, 4 `{ struct T; }` (`mod a`'s braces are its item's: not scanned).
        assert_eq!(got, std::vec![None, Some(0), Some(1), None, Some(2)]);
        let (rendered, _) = statements_as_items(text, call(), alloc).unwrap_or_else(|e| panic!("read: {e}")).expect("it declares items");
        let tag = std::format!("{:016x}", tag(call()));
        let zero = rendered.find(&std::format!("mod {EXPANSION_BLOCK}{tag}_0 {{")).expect("block 1 is `_0`");
        let one = rendered.find(&std::format!("mod {EXPANSION_BLOCK}{tag}_1 {{")).expect("block 2 is `_1`");
        let two = rendered.find(&std::format!("mod {EXPANSION_BLOCK}{tag}_2 {{")).expect("block 4 is `_2`");
        assert!(zero < one && one < two);
        assert!(!rendered.contains(&std::format!("{tag}_3")));
        // `_1` is inside `_0`: no line `}` (a written block's close) between `_0`'s open and `_1`'s.
        assert!(!rendered[zero..one].lines().any(|l| l == "}"));
    }

    /// An expression with no item and no statement call declares nothing. Prints nothing.
    #[test]
    fn an_expression_declares_nothing() {
        let alloc = blaze_own::testing::heap().alloc();
        let got = statements_as_items("if !x { ::core::panicking::panic(\"x\") }", call(), alloc).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(got.is_none());
        assert_eq!(statements_as_items("{ mod m { }", call(), alloc).err(), Some(StatementsRefused::Unbalanced));
    }
}
