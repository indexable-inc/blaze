//! One table per lowered body: for each block scope of the body, the module of the names
//! library its names are read in. Built once, when the body's local items are kept
//! (`Locals::keep`), and again for a pattern's question while the body is lowered
//! (`glob_uses::by_uses`, before the items are kept). Every reader that places a local item,
//! or reads a block `use`'s path at a block, takes its module from here, by the SCOPE the item
//! or the path stands in. Nothing here looks at where a site falls between macro calls.
//!
//! # The rules
//!
//! The Reference, "Statements", "Item declarations" [statement.item.scope]: "Declaring an item
//! within a statement block restricts its scope to the block containing the statement."
//! "Names", "Scopes", "Item scopes": "The name of an item declared directly in a block is in
//! scope for the entire block" (before its declaration too). "Use declarations": "Use
//! declarations may appear in modules and blocks, usually at the top." "Macros by example",
//! "Hygiene": "`macro_rules` macros have mixed-site hygiene. This means that loop labels,
//! block labels, and local variables are looked up at the macro definition site while other
//! symbols are looked up at the macro invocation site." "Macros", "Macro invocation": a macro
//! may be invoked as a statement, and its expansion is then statements of the calling block.
//!
//! So, for `macro_rules!` (the only expander that writes body statements today):
//! - An item NAME an expansion writes is not hygienic: it is an item of the block its
//!   statement stands in. Where an expansion's top-level statement is an item, that block is
//!   the CALLING block: the item is visible to the caller's code in the whole calling block,
//!   after the call and before it. Where it is inside a block the expansion wrote (`{ mod
//!   __select_util { .. } use __select_util::Out; .. }`), it is an item of THAT block, seen
//!   only inside it.
//! - A path's first segment `n` (not `self`, `super`, `crate`, `$crate` or `::`), written in
//!   scope `s`, is read in this order: the items and named imports of `s`, then the globs of
//!   `s` (an item or named import shadows a glob of its block: super::glob_uses), then the
//!   same for each block around `s`, outward; then the body's module. A block an expansion
//!   wrote is a block in that chain like any hand block. The text that wrote a block decides
//!   which block it is. Hygiene decides nothing for item names.
//! - What a found item continues into is read at the module of the block that declares it:
//!   this table's [`ScopeAt::Own`] module of the item's scope, the module the names library
//!   files for that block (blaze-scope blocks.rs for a hand block, expanded_blocks.rs for an
//!   expansion's). A block `use` is read where it is written: its path's first segment goes
//!   through the same order from the `use`'s scope (super::world_paths, super::glob_uses).
//! - A name that no block declares is the module's. That is the scope chain's last step, not a
//!   guess. An item of a block whose module the library did not file (an expansion that is not
//!   a statement call, a local item's own body) is refused by name
//!   (`LocalKind::unplaced`), never read at the calling block. The calling block's module is
//!   NOT an answer for an expansion's block: two `{ mod m { .. } }` blocks of one expansion, or
//!   of two calls, are two modules.
//!
//! In tests/front2_macro_block_mods.rs `select_shaped`, `Out::A(5)`, `Out::A(x)` and
//! `Out::B` are all the MACRO's own tokens (the transcriber of `select_like!`). Of the
//! caller's tokens only `$out`'s `out` is in the expansion, and it is a local variable. Those
//! three paths stand in the block the expansion wrote. Their `Out` is that block's named
//! import `use __select_util::Out;`, whose `__select_util` is that block's own `mod`. So all
//! three resolve at the block's module, `__blaze_block_x<tag>_0` under the call. The crate's
//! own items are seen there only through the module's `use super::*` chain, after the block's
//! own.
//!
//! # The identities
//!
//! A scope's row says which text wrote its `{` (blaze-lower scope_origin.rs: the lowering
//! reads it off the word, whose expansion index it carries). The module the names library files
//! for a block is keyed by WHERE the writing call stands, never by the call's content:
//! `blaze_expand::call_key` is one key for two equal calls (they share one kept expansion),
//! while two equal calls in one block are two modules with two `Out` enums. The names
//! library keys a statement call `key_of(krate, owner, "MacroCall", "", n)`: `owner` is the
//! module of the hand block that holds it (blocks::block_modules), or the writing call's key
//! for a call at an expansion's top level (the call's fragment), or the module of the
//! expansion's block that holds it; `n` is its place among that owner's statement calls. The
//! `ordinal`th written block of a call `k`'s expansion is named
//! `expanded_blocks::expansion_block_name(k, ordinal)`. Its key is `key_of(krate, outer,
//! "mod", name, 0)`, where `outer` is `k` for a block at the expansion's top level and the
//! module of the written block around it otherwise (a made module keys its inner `mod`s under
//! itself: blaze-scope task/lower.rs). Each step here derives the same key from the same scan
//! of the same expansion text (`expanded_blocks::expansion_blocks`, the one place that
//! numbers an expansion's blocks).

use blaze_file_items::nested::Nested;
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_lower::scope_origin::{CallAt, ScopeOrigin};
use blaze_lower::{ScopeId, TextAt, View};
use blaze_own::{AVec, Alloc};
use blaze_scope::expanded_blocks::{ExpansionBlocks, WRAP_OPEN, expansion_block_name, expansion_blocks};
use blaze_scope::task::key_of;

/// Where the names written in one block scope of a body are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeAt {
    /// The block is a module of the names library: its own items are that module's.
    Own(LibKey),
    /// The block declares nothing that makes a module (nor holds a statement call): its names
    /// are its parent's, read at this module.
    Inherits(LibKey),
    /// The block would need a module and the names library filed none (it is written by an
    /// expansion of a call in expression position, or the body is a local item's own): names
    /// that are not its own items read at its parent's module, given here; its own
    /// module-making items are refused by name (`LocalKind::unplaced`).
    Unfiled(LibKey),
}

impl ScopeAt {
    /// The module a name written in the block, not one of its own items, is read at.
    pub fn module(self) -> LibKey {
        match self {
            ScopeAt::Own(m) | ScopeAt::Inherits(m) | ScopeAt::Unfiled(m) => m,
        }
    }

    /// The block's own module, where its module-making items are; `None` when it has none.
    pub fn own(self) -> Option<LibKey> {
        match self {
            ScopeAt::Own(m) => Some(m),
            ScopeAt::Inherits(_) | ScopeAt::Unfiled(_) => None,
        }
    }
}

/// A planted bug for the test that must catch it (rule 8): the table answers the CALLING
/// block's module for a block an expansion wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Planted {
    #[default]
    None,
    CallingModuleForExpansions,
}

/// Why a body's scope table was not built, naming the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeRefused {
    /// A row of the lowered record did not read (a record whose bytes are wrong).
    Record(TextAt),
    /// A scope's `{` is no brace group of the scan of the text that wrote it: the lowering and
    /// the names library's scan disagree about the block.
    ScopeNotScanned { scope: u32 },
    /// A scope's parent is not written before it (the lowering writes scopes outermost first).
    ParentAfterChild { scope: u32 },
    /// An expansion's text does not balance its brackets: its blocks cannot be numbered
    /// (`expanded_blocks::StatementsRefused::Unbalanced`).
    ExpansionUnbalanced { expansion: u32 },
}

impl ScopeRefused {
    /// The refusal's words (a front2 `Why::Unsupported` form).
    pub fn form(self) -> &'static str {
        match self {
            ScopeRefused::Record(_) => "a lowered body whose scope rows do not read",
            ScopeRefused::ScopeNotScanned { .. } => {
                "a block of a body that the scan of the text that wrote it does not see: which module the block is cannot be read (statement.item.scope)"
            }
            ScopeRefused::ParentAfterChild { .. } => "a lowered body whose block scopes are not written outermost first",
            ScopeRefused::ExpansionUnbalanced { .. } => {
                "a macro expansion in a body whose brackets do not balance: its blocks (the scope of the items it declares) cannot be read"
            }
        }
    }
}

impl From<TextAt> for ScopeRefused {
    fn from(t: TextAt) -> ScopeRefused {
        ScopeRefused::Record(t)
    }
}

impl From<blaze_lower::BadRow> for ScopeRefused {
    fn from(b: blaze_lower::BadRow) -> ScopeRefused {
        ScopeRefused::Record(TextAt::Row(b))
    }
}

/// A body's scope table: per lowered scope (by `ScopeId`), where its names are read, and the
/// hand block of the body's scan it is (`None` for the outermost scope and a block an
/// expansion wrote).
pub struct ScopeTable<'a> {
    pub scopes: AVec<'a, ScopeAt>,
    pub hand_blocks: AVec<'a, Option<u32>>,
}

impl ScopeTable<'_> {
    /// Where the names of scope `s` are read; `None` for a scope the body does not have.
    pub fn at(&self, s: u32) -> Option<ScopeAt> {
        self.scopes.get(s as usize).copied()
    }
}

/// One expansion the table reads: its blocks as the names library numbers them, the names key
/// of its call (`None`: the call is not a statement call of a block the library filed), and
/// the module of each of its blocks (`None` for block 0, the calling block, and every block
/// not written).
struct Expanded<'a> {
    blocks: ExpansionBlocks<'a>,
    call: Option<LibKey>,
    modules: AVec<'a, Option<LibKey>>,
}

/// The statement call of `nested` that starts at `start` (an exact place, never a range
/// holding it): its block and its place among that block's statement calls. The lowering's
/// call node starts at its first outer attribute or at its path, and the scan's call at its
/// statement's start or its path: each is one of THIS call's own anchors, which no other
/// call shares.
fn statement_call(nested: &Nested<'_>, start: u32) -> Option<(u32, u32)> {
    let i = nested.calls.iter().position(|c| c.range.start == start || c.path.start == start)?;
    let block = nested.calls[i].block;
    let n = nested.calls[..i].iter().filter(|c| c.block == block).count() as u32;
    Some((block, n))
}

/// The scope table of the body lowered to `view`: its crate `krate`, its module
/// `body_module`, the scan of its item's text `nested`, and each scanned block's module
/// (`blaze_scope::blocks::block_modules`), `None` when the body is no item or member of the
/// library (a local item's own body). `alloc` holds the table and the expansions' scans.
pub fn build<'a>(
    krate: CrateItemKey,
    body_module: LibKey,
    view: &View<'_>,
    nested: &Nested<'_>,
    hand: Option<&[Option<LibKey>]>,
    planted: Planted,
    alloc: Alloc<'a>,
) -> Result<ScopeTable<'a>, ScopeRefused> {
    let scopes = view.scopes.len();
    let expansions = view.expansion_keys.len();
    // The expansions a scope stands in, and every expansion whose text holds such a call:
    // the call's key needs its writer's blocks.
    let mut needed: AVec<'a, bool> = AVec::new_in(alloc);
    needed.resize(expansions, false);
    for s in 0..scopes {
        let ScopeOrigin::Expansion { expansion, .. } = view.scope_origin(ScopeId(s as u32))? else { continue };
        let mut e = expansion;
        // Each step goes to an earlier expansion (`View::expansion_call_at`): at most one step
        // per expansion.
        for _ in 0..=expansions {
            match needed.get_mut(e as usize) {
                Some(slot) if !*slot => *slot = true,
                Some(_) => break,
                None => return Err(ScopeRefused::Record(TextAt::Row(blaze_lower::BadRow { column: "scopes", tag: u16::MAX }))),
            }
            match view.expansion_call_at(e)? {
                CallAt::Item { .. } => break,
                CallAt::Expansion { expansion: writer, .. } => e = writer,
            }
        }
    }
    // Each needed expansion in order: a writer's index is below its calls' (the View's rule),
    // so its key and modules are made first.
    let mut made: AVec<'a, Option<Expanded<'a>>> = AVec::new_in(alloc);
    for j in 0..expansions {
        if !needed[j] {
            made.push(None);
            continue;
        }
        let j32 = j as u32;
        let text = view.expansion_text(j32)?;
        let blocks = expansion_blocks(text, alloc).map_err(|_| ScopeRefused::ExpansionUnbalanced { expansion: j32 })?;
        let call = match view.expansion_call_at(j32)? {
            CallAt::Item { start } => statement_call(nested, start).and_then(|(block, n)| {
                let owner = hand?.get(block as usize).copied().flatten()?;
                Some(key_of(krate, owner, "MacroCall", "", n))
            }),
            CallAt::Expansion { expansion: writer, start } => match made.get(writer as usize).and_then(|w| w.as_ref()) {
                Some(w) => w.call.and_then(|k| {
                    let (block, n) = statement_call(&w.blocks.nested, start + WRAP_OPEN)?;
                    // A call at the writer's top level is a call of the writer's fragment,
                    // keyed under the writer's call; one in a block, under that block's module.
                    let owner = (if block == 0 { Some(k) } else { w.modules.get(block as usize).copied().flatten() })?;
                    Some(key_of(krate, owner, "MacroCall", "", n))
                }),
                None => None,
            },
        };
        let mut modules: AVec<'a, Option<LibKey>> = AVec::new_in(alloc);
        modules.resize(blocks.nested.blocks.len(), None);
        if let Some(k) = call {
            for b in 0..blocks.nested.blocks.len() {
                let Some(ordinal) = blocks.ordinals.get(b).copied().flatten() else { continue };
                // A written block's parent is written (or is block 0) and comes first.
                let outer = match blocks.nested.blocks[b].parent {
                    None | Some(0) => Some(k),
                    Some(p) => modules.get(p as usize).copied().flatten(),
                };
                let name = expansion_block_name(k, ordinal, alloc);
                modules[b] = outer.map(|o| key_of(krate, o, "mod", name.as_str(), 0));
            }
        }
        made.push(Some(Expanded { blocks, call, modules }));
    }
    let mut table = ScopeTable { scopes: AVec::new_in(alloc), hand_blocks: AVec::new_in(alloc) };
    for s in 0..scopes {
        let id = ScopeId(s as u32);
        let parent = match view.scope(id)?.parent {
            None => body_module,
            Some(p) => match table.scopes.get(p.0 as usize) {
                Some(at) => at.module(),
                None => return Err(ScopeRefused::ParentAfterChild { scope: s as u32 }),
            },
        };
        let (at, hand_block) = match view.scope_origin(id)? {
            ScopeOrigin::Root => (ScopeAt::Inherits(body_module), None),
            ScopeOrigin::Item { open } => {
                let b = nested.blocks.iter().position(|blk| blk.open == open).ok_or(ScopeRefused::ScopeNotScanned { scope: s as u32 })?;
                let at = match hand {
                    Some(h) => match h.get(b).copied().flatten() {
                        Some(m) => ScopeAt::Own(m),
                        None => ScopeAt::Inherits(parent),
                    },
                    None => ScopeAt::Unfiled(parent),
                };
                (at, Some(b as u32))
            }
            ScopeOrigin::Expansion { expansion, open } => {
                let e = made.get(expansion as usize).and_then(|e| e.as_ref()).ok_or(ScopeRefused::ScopeNotScanned { scope: s as u32 })?;
                let b = e.blocks.nested.blocks.iter().position(|blk| blk.open == open + WRAP_OPEN).ok_or(ScopeRefused::ScopeNotScanned { scope: s as u32 })?;
                let at = match (e.blocks.ordinals.get(b).copied().flatten(), e.modules.get(b).copied().flatten()) {
                    (None, _) => ScopeAt::Inherits(parent),
                    (Some(_), Some(_)) if planted == Planted::CallingModuleForExpansions => ScopeAt::Own(parent),
                    (Some(_), Some(m)) => ScopeAt::Own(m),
                    (Some(_), None) => ScopeAt::Unfiled(parent),
                };
                (at, None)
            }
        };
        table.scopes.push(at);
        table.hand_blocks.push(hand_block);
    }
    Ok(table)
}
