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
//! A local item replaced by an attribute contributes the items in its replacement to its
//! block. The body lowering sees the annotated item in the input, while the names library
//! sees the replacement in the block's module.
//!
//! For `macro_rules!`:
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

/// Where a body's blocks' own names are: D189 step 4's seam (rule 78), matched at ONE place,
/// [`block_binding`]. Under `BlockScopes::Modules` (the default) the crate solve made each
/// block a module of its crate, read through the names source like any module; under
/// `BlockScopes::Bodies` the crate's names hold no block, and the body's `BodyScope` record
/// (blaze-scope body_scope.rs) holds each block's scope by the same module key (the one
/// `make_blocks` names both).
#[derive(Clone, Copy, Debug)]
pub enum BlockNames<'r, 'a> {
    Modules,
    /// The body's `BodyScope` record as the store holds it, read in place.
    Bodies(&'r blaze_scope::body_scope::BodyScopeView<'a>),
}

impl BlockNames<'_, '_> {
    /// The strategy this answers by.
    pub fn strategy(self) -> blaze_scope::surface::BlockScopes {
        match self {
            BlockNames::Modules => blaze_scope::surface::BlockScopes::Modules,
            BlockNames::Bodies(_) => blaze_scope::surface::BlockScopes::Bodies,
        }
    }
}

/// The direct binding of `name` in `ns` in the block whose module is `module`, before its globs:
/// the outer option says whether the block's scope was read (`None`: the block is no module of
/// the names under `Modules`, or no block of the body under `Bodies`: a gap, never "unbound").
pub fn block_binding<'n, S: super::source::NamesSource<'n>>(
    names: S,
    blocks: BlockNames<'_, '_>,
    module: LibKey,
    ns: blaze_surface::Namespace,
    name: &str,
) -> Option<Option<blaze_surface::Binding>> {
    match blocks {
        BlockNames::Modules => names.block_binding(module, ns, name),
        BlockNames::Bodies(body) => body.binding(module, ns, name),
    }
}

/// A name whose first segment is looked up in the block that contains the invocation.
pub(super) fn invocation_name(absolute: bool, first: &str, mark: Option<blaze_lower::mark_site::TextMark>) -> bool {
    !absolute
        && !matches!(first, "self" | "super" | "crate" | "$crate" | "Self" | "{{root}}" | "<qualified>")
        && !mark.is_some_and(|m| m.transparency == blaze_expand::Transparency::Opaque)
}

/// A direct binding that the lowerer's syntax rows did not list. It enters the existing
/// block walk at its own lexical depth, before that block's globs. The solver reads the
/// unchanged path from this module, including any remaining segments. The body's blocks are
/// read by `blocks` (D189's seam: the body's `BodyLocals::names_of_blocks`).
#[expect(clippy::too_many_arguments, reason = "direct_name's arguments and the seam")]
pub(super) fn direct_name_in<'a, 'n, S: super::source::NamesSource<'n>>(
    body: &super::locals::BodyLocals<'_>,
    scope: u32,
    name: &'a str,
    ns: crate::resolve::Ns,
    depth: u32,
    alloc: Alloc<'a>,
    names: S,
    blocks: BlockNames<'_, '_>,
) -> Result<Option<super::glob_uses::UseAt<'a>>, super::locals::LocalGap> {
    use super::glob_uses::{Start, UseAt};
    let place = body.scopes.get(scope as usize).expect("the enclosing body's recorded ScopeId indexes its scope table");
    let Some(module) = place.own() else { return Ok(None) };
    let ns = match ns {
        crate::resolve::Ns::Type => blaze_surface::Namespace::Types,
        crate::resolve::Ns::Value => blaze_surface::Namespace::Values,
    };
    let range = blaze_surface::Range::new(0, 0);
    let bound = block_binding(names, blocks, module, ns, name).ok_or(super::locals::LocalGap { item: module, form: "the names scope of the block containing the path", range })?;
    let Some(bound) = bound else { return Ok(None) };
    if blaze_scope::blocks::is_block_module(name) && body.scopes.iter().any(|place| place.own() == Some(bound.def.key)) {
        return Ok(None);
    }
    let start = Start { at: module, abs: false, dollar: None, path: AVec::from_slice_in(&[name], alloc) };
    Ok(Some(UseAt { depth, bound: Some(name), start: Some(start), written: AVec::new_in(alloc), item: module, range }))
}

/// The first segment of an expression path. A local item keeps its body's key and shadows an
/// outer name before mark-based lookup applies. A binding in a nearer block shadows an outer
/// syntax row.
#[expect(clippy::too_many_arguments, reason = "the path's first word and site, and the enclosing body")]
pub(super) fn value_prefix<'n, 'l: 'a, 'a, S: super::source::NamesSource<'n>>(
    paths: &crate::resolve::Paths<S>,
    locals: &super::locals::Locals<'l>,
    owner: blaze_typeck::ItemKey,
    module: LibKey,
    scope: Option<u32>,
    first: &'a str,
    single: bool,
    mark: Option<blaze_lower::mark_site::TextMark>,
    alloc: Alloc<'a>,
) -> Result<(Option<super::locals::Row<'l>>, Option<super::glob_uses::Hit<'a>>), super::locals::LocalGap> {
    use super::locals::{LocalKind, LocalNs};
    let ns = if single { LocalNs::Value } else { LocalNs::Type };
    let mut row = locals.find(owner, scope, first, ns);
    if row.is_none() && single {
        row = locals.find(owner, scope, first, LocalNs::Type)
            .filter(|r| r.kind.in_block_module() && !matches!(r.kind, LocalKind::Mod | LocalKind::ExternCrate));
    }
    if !invocation_name(false, first, mark) {
        // Item names in a block use the block's lexical scope even when the token came from a
        // macro definition. Mixed-site hygiene applies to locals and labels, not to item names
        // (the Reference, "Macros by example", "Hygiene"). Keep a local item found above, but
        // do not inspect invocation-site imports or globs for an opaque path.
        return Ok((row, None));
    }
    let Some(scope) = scope else { return Ok((row, None)) };
    let ns = if single { crate::resolve::Ns::Value } else { crate::resolve::Ns::Type };
    let hit = super::glob_uses::before(locals, paths, module, owner, scope, first, ns, row.as_ref(), alloc)?;
    Ok((if hit.is_none() { row } else { None }, hit))
}

/// A body block supplies the first segment of a plain invocation-site path when the
/// enclosing blocks bind that name. A named import stands for its target, and an item is
/// read in its declaring block's module, even before its declaration. The first token's
/// mark decides whether invocation-site lookup applies at all: an opaque name uses its
/// definition site. A rooted path (including a marked `crate` for `$crate`) or a
/// `self`/`super` path starts at its designated root or lexical module. Those paths, and
/// plain names outside the block bindings, are left to [`super::item_site::resolve_written`].
/// Thus block lookup refines only the invocation site's unqualified names.
#[expect(clippy::too_many_arguments, reason = "one path of the builder, its body and the namespace it asks")]
pub(super) fn written<'n, S: super::source::NamesSource<'n>>(
    resolver: &crate::resolve::Paths<S>,
    marks: Option<blaze_scope::crate_roots::TextMarks<'_>>,
    body: &super::block_type_uses::BodyScope<'_>,
    view: &View<'_>,
    path: blaze_typeck::PathIdx,
    absolute: bool,
    segments: &[&str],
    ns: crate::resolve::Ns,
    at: blaze_surface::Range,
) -> Result<Option<crate::resolve::Target>, super::types::Why> {
    use super::locals::{LocalKind, LocalNs};
    use super::types::Why;

    let Some(&first) = segments.first() else { return Ok(None) };
    // A path of no block scope is a signature's. A LOCAL item's signature (a fn, const or
    // static declared in a body, its head or its body lowered) stands in the block it is
    // declared in (the Reference, "Statements", "Item declarations": "Declaring an item within
    // a statement block restricts its scope to the block containing the statement"): asked
    // as the enclosing body's, at the declaring block's scope (`LocalHead::enclosing`,
    // `LocalHead::scope`), one step out, as `Locals::find` steps out of a local's body. A
    // block of only `use` items and fns is no module of the names library, so its import was
    // read nowhere else (alloc's `custom_owner_from_box`: `use core::ptr::NonNull;` then
    // `unsafe fn drop_box_raw<T: ?Sized>(ptr: NonNull<T>)`: E0412; ladder rungs f3-dyn,
    // x0-ref-11-deref-unsized, x0-ref-12-wide-ptr-eq; tests/front2_block_use_types.rs). Any
    // other body's signature path is its module's.
    let declared;
    let (body, scope) = match view.path(path)?.scope {
        Some(s) => (body, s.0),
        None => match body.locals.head(body.owner) {
            Some(h) => {
                declared = super::block_type_uses::BodyScope { owner: h.enclosing, ..*body };
                (&declared, h.scope)
            }
            None => return Ok(None),
        },
    };
    // Body lowering already put the first token's mark in the path's view. An opaque
    // token names the definition module even if that module also encloses the invocation.
    if !invocation_name(absolute, first, view.mark_of_path(path)?) {
        return Ok(None);
    }
    // The first segment's namespace: the asked one for a one-segment path, else the type
    // namespace (the Reference, "Paths": every segment before the last names a module, a type or
    // a trait), as `value_prefix` reads it. A one-name const argument asked in the value
    // namespace (types.rs `path_args`) read a block's `const` in the type namespace, found
    // nothing and fell to the module.
    let first_ns = if segments.len() == 1 { ns } else { crate::resolve::Ns::Type };
    let local_ns = match first_ns {
        crate::resolve::Ns::Type => LocalNs::Type,
        crate::resolve::Ns::Value => LocalNs::Value,
    };
    let row = body.locals.find(body.owner, Some(scope), first, local_ns);
    let hit = super::glob_uses::before(body.locals, resolver, body.module, body.owner, scope, first, first_ns, row.as_ref(), body.alloc)
        .map_err(|gap| Why::Unsupported(gap.form, gap.range))?;
    if let Some(hit) = hit {
        // A direct declaration keeps the original first segment. A glob prepends its path.
        if !hit.abs && hit.dollar.is_none() && hit.path.as_slice() == [first] {
            return super::item_site::resolve_written(resolver, marks, hit.at, view, path, absolute, segments, ns, at).map(Some);
        }
        let mut through = hit.path;
        let prefix = through.len() as u32;
        through.extend_from_slice(&segments[1..]);
        let target = resolver.path_with(hit.at, hit.dollar, hit.abs, &through, ns).map_err(|e| Why::Resolve(e, at))?;
        let rest = target.consumed.checked_sub(prefix).ok_or(Why::Unsupported(super::glob_uses::TYPE_THROUGH_GLOB, at))?;
        return Ok(Some(crate::resolve::Target { consumed: 1 + rest, ..target }));
    }
    let Some(row) = row else { return Ok(None) };
    if row.kind == LocalKind::Use {
        return super::block_type_uses::through_use(resolver, marks, body, scope, view, path, absolute, segments, ns, at);
    }
    let module = row.module.ok_or(Why::Unsupported(row.kind.unplaced(), at))?;
    super::item_site::resolve_written(resolver, marks, module, view, path, absolute, segments, ns, at).map(Some)
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
pub(super) struct Expanded<'a> {
    pub(super) blocks: ExpansionBlocks<'a>,
    pub(super) call: Option<LibKey>,
    pub(super) modules: AVec<'a, Option<LibKey>>,
}

/// A body's expansions as [`expanded`] reads them: each one's text and where its call stands.
/// Two sources give them: the finished record's [`View`] (the table, built after the
/// lowering), and the lowering's expansions so far, [`blaze_lower::Expansions`] (the macro
/// resolver, front2 scoped.rs, which reads a call's block while the body is lowered). Both
/// read the same columns, so a block is keyed the same way by both.
pub(super) trait ExpansionTexts {
    fn text_of(&self, j: u32) -> Result<&str, ScopeRefused>;
    fn call_of(&self, j: u32) -> Result<CallAt, ScopeRefused>;
}

impl ExpansionTexts for View<'_> {
    fn text_of(&self, j: u32) -> Result<&str, ScopeRefused> {
        Ok(self.expansion_text(j)?)
    }

    fn call_of(&self, j: u32) -> Result<CallAt, ScopeRefused> {
        Ok(self.expansion_call_at(j)?)
    }
}

impl ExpansionTexts for blaze_lower::Expansions<'_> {
    fn text_of(&self, j: u32) -> Result<&str, ScopeRefused> {
        self.text(j).ok_or(ScopeRefused::Record(TextAt::Row(blaze_lower::BadRow { column: "expansion_starts", tag: u16::MAX })))
    }

    fn call_of(&self, j: u32) -> Result<CallAt, ScopeRefused> {
        self.call_at(j).ok_or(ScopeRefused::Record(TextAt::Row(blaze_lower::BadRow { column: "expansion_calls", tag: u16::MAX })))
    }
}

/// Expansion `j`'s blocks as the names library numbers and keys them ([`Expanded`]): its
/// call's key from where the call stands, in the item's text `nested` (whose blocks' modules
/// are `hand`) or in its writer's expansion, which `made` holds at the writer's index (a
/// writer's index is below its calls', so it is made first); then each written block's
/// module under that key. The module doc's "The identities" states the keys.
pub(super) fn expanded<'a, E: ExpansionTexts + ?Sized>(
    krate: CrateItemKey,
    nested: &Nested<'_>,
    hand: Option<&[Option<LibKey>]>,
    texts: &E,
    j: u32,
    made: &[Option<Expanded<'a>>],
    alloc: Alloc<'a>,
) -> Result<Expanded<'a>, ScopeRefused> {
    let text = texts.text_of(j)?;
    let blocks = expansion_blocks(text, alloc).map_err(|_| ScopeRefused::ExpansionUnbalanced { expansion: j })?;
    let call = match texts.call_of(j)? {
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
    Ok(Expanded { blocks, call, modules })
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
        let one = expanded(krate, nested, hand, view, j as u32, &made, alloc)?;
        made.push(Some(one));
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
