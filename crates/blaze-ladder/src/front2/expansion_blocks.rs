//! The module of a local item that a macro expansion in statement position wrote: the half of
//! "block modules from expansions" that belongs to front2 (blaze-scope expanded_blocks.rs is
//! the names library's half).
//!
//! The Reference, "Statements", "Item declarations": "Declaring an item within a statement
//! block restricts its scope to the block containing the statement." "Macros by example",
//! "Hygiene": items an expansion declares are looked up at the invocation site. So an item
//! that `tokio::select!` writes (`{ mod __tokio_select_util { .. } use .. ; .. }`) is an item
//! of a block, and the names library files that block as a module: a block the expansion
//! writes is a module named `expanded_blocks::expansion_block_name(call, ordinal)`, declared in
//! the fragment of the statement call `call` (its key is `(calling block's module, "mod",
//! name, 0)` under the call's key); an item at the expansion's top level is the calling
//! block's own (the calling block's module, `blocks::block_modules(..)[call.block]`).
//!
//! The statement call's names key is derived as the names library derives it: the call is a
//! `MacroCall` item of its block's module, keyed `(module, "MacroCall", "", n)` where `n` is
//! its place among that block's direct statement calls in text order (blaze-scope task/lower.rs
//! `Lower::key` and `kind_tag`; a call carries no name).
//!
//! Which block of the expansion a row is in is decided by which of those modules BINDS the
//! row's name, the same test the hand-written case uses (`NamesSource::binds`): exactly one
//! written block of the expansion binds it: that block's module; none: the calling block's
//! module when it binds it; more than one: refused by name (two blocks of one expansion
//! declaring one name cannot be told apart by name; the lowering's scopes would be needed).

use blaze_file_items::nested::Nested;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_mem::Frame;
use blaze_own::Alloc;
use blaze_surface::Namespace;

use super::source::NamesSource;

/// The most written blocks of one expansion looked through: a bound, not a guess (an
/// expansion with more is refused by name).
pub const MOST_EXPANSION_BLOCKS: u32 = 1024;

/// Why the module of an expansion's local item was not read, naming the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpansionModuleRefused {
    /// Two written blocks of one statement call's expansion bind the row's name.
    TwoBlocks,
    /// The expansion has more written blocks than [`MOST_EXPANSION_BLOCKS`].
    TooManyBlocks,
}

impl ExpansionModuleRefused {
    /// The refusal's words (a front2 `Why::Unsupported` form).
    pub fn form(self) -> &'static str {
        match self {
            ExpansionModuleRefused::TwoBlocks => {
                "an item name declared in two blocks of one macro expansion: which block a local item is in is not read by name (statement.item.scope)"
            }
            ExpansionModuleRefused::TooManyBlocks => "a macro expansion with more written blocks than front2 looks through",
        }
    }
}

/// The names key of the statement call `nested.calls[call]` of a body whose blocks' modules are
/// `modules` (`blocks::block_modules`), or `None` when its block has no module.
pub fn statement_call_key(krate: CrateItemKey, nested: &Nested<'_>, modules: &[Option<ItemKey>], call: usize) -> Option<(ItemKey, ItemKey)> {
    let c = nested.calls.get(call)?;
    let module = modules.get(c.block as usize).copied().flatten()?;
    let n = nested.calls[..call].iter().filter(|d| d.block == c.block).count() as u32;
    Some((module, blaze_scope::task::key_of(krate, module, "MacroCall", "", n)))
}

/// The module of a local item named `name` in namespace `ns` that an expansion wrote, whose
/// site (`View::site`: the outermost call's range, item-relative) starts at `site`: the block
/// module the names library filed for it, or `None` when no statement call of the body holds
/// the site or no module binds the name (the caller refuses it as before,
/// `LocalKind::unplaced`).
#[allow(clippy::too_many_arguments, reason = "the body's scan, its modules and the row: one call site, in Locals::keep")]
pub fn module_of<'n, S: NamesSource<'n>>(
    names: S,
    frame: &'n Frame,
    krate: CrateItemKey,
    nested: &Nested<'_>,
    modules: &[Option<ItemKey>],
    site: u32,
    ns: Namespace,
    name: &'n str,
    alloc: Alloc<'_>,
) -> Result<Option<ItemKey>, ExpansionModuleRefused> {
    let Some(call) = nested.calls.iter().position(|c| c.range.start <= site && site < c.range.end) else {
        return Ok(None);
    };
    let Some((calling, call_key)) = statement_call_key(krate, nested, modules, call) else {
        return Ok(None);
    };
    let mut found: Option<ItemKey> = None;
    let mut ordinal = 0u32;
    loop {
        if ordinal == MOST_EXPANSION_BLOCKS {
            return Err(ExpansionModuleRefused::TooManyBlocks);
        }
        let block = blaze_scope::expanded_blocks::expansion_block_name(call_key, ordinal, alloc);
        // `binds` takes a name of the source's lifetime: a copy in the command's frame.
        let block: &'n str = frame.alloc_str(block.as_str());
        // The expansion's written blocks are declared in the call's fragment, in the calling
        // block's module, in order: the first ordinal not bound there ends them.
        if !names.binds(calling, Namespace::Types, block) {
            break;
        }
        let module = blaze_scope::task::key_of(krate, call_key, "mod", block, 0);
        if names.binds(module, ns, name) {
            if found.is_some() {
                return Err(ExpansionModuleRefused::TwoBlocks);
            }
            found = Some(module);
        }
        ordinal += 1;
    }
    Ok(found.or_else(|| names.binds(calling, ns, name).then_some(calling)))
}
