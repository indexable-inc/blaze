//! A body's macro calls resolved through a names source: [`ScopedBy`] is blaze-lower's
//! `scoped::Scoped` (the [`MacroNames`] of one module) over [`NamesSource`], so a macro call in
//! a body or a signature expands the same over the solved names and over each crate's records.
//!
//! The rules are `Scoped`'s, unchanged (the Reference, "Macros By Example", Hygiene: "By
//! default, all identifiers referred to in a macro expansion are looked up as if they appeared
//! at the invocation site", with `$crate` the exception, "the crate where the macro was
//! defined"; a `macro` is def-site, its names resolved where it is written):
//! - a path the body writes: in the body's module;
//! - a path an expansion wrote: `$crate::m` in the crate the expansion's mark names; a path
//!   under an opaque mark (a `macro`'s) in the definition's module; every other path at the call
//!   site, the body's module, as a written one.
//!
//! One question is asked of the source, [`NamesSource::macro_at`]: the item the path names in
//! the macro namespace, its lowered source (a `macro_rules!`' or `macro`'s body, a builtin's
//! name, a proc macro's kind) and the defining crate's key, proc-macro key and edition. Over
//! records that is one walk through the names records and one macro row of the defining crate's
//! items record, whose body text is read from its file and checked against the row's digest.
//! The definition is then built by blaze-scope's `definition_of`, the one place a macro's source
//! becomes a definition (the names fill and `Names::macro_definition` build theirs there too).
//!
//! A `macro_rules!` or `macro` is lexed and compiled once per build, not at each call:
//! [`Prepares`] holds each by the key of the item that defines it.

use blaze_expand::{DefAt, DefSite, Export, LocalInner, MacroDefinition, ProcKind};
use blaze_id::ItemKey;
use blaze_lower::by_name::{MacroNames, NoMacro, PathAt, Resolved};
use blaze_own::{AVec, Alloc, Vec};
use blaze_scope::names::Written;
use blaze_scope::prepares::{Kept, Prepares};
use blaze_scope::resolve::{MacroDefError, definition_of};
use blaze_scope::task::{MacroBody, MacroSrc};
use blaze_scope::{CallShape, PathKind};

use super::source::NamesSource;

/// The macro names of one module, from the source `S`.
pub struct ScopedBy<'d, 'n: 'd, S> {
    /// The source; every definition built from it lives for `'n` (the command's).
    pub names: S,
    /// The module the body is in. The `macro_rules!` the body itself writes are in textual
    /// scope before any of these names: the lowering's driver hands them to each call.
    pub module: ItemKey,
    /// The definitions prepared so far, shared by every body over `names`; `None` prepares a
    /// definition at each call (a signature's type macro, which has no place to keep them).
    pub prepared: Option<&'d Prepares<'n>>,
    /// The body's blocks as the names library made them modules ([`body_blocks`]). A call-site
    /// path is read in the innermost block around its call: a block of the body's text
    /// ([`BlockModules::hand_module`]) or a block an expansion wrote
    /// ([`BlockModules::expansion_module`]). That block's module declares the block's items
    /// (blaze-scope blocks.rs, expanded_blocks.rs) and reaches the blocks around it and the
    /// module through its `use super::*`. `None` for a text with no block of the names library
    /// (a signature, a local item's own body).
    pub blocks: Option<&'d dyn BlockModules>,
}

/// A body's blocks as the names library made them modules. For the body's own text: the scan
/// of the text and each scanned block's module (blaze-scope `blocks::block_modules`, as front2
/// locals.rs `Locals::keep` names them). For the expansions lowered in it: each expansion's
/// blocks, keyed by front2 scope_table.rs `expanded`, the one derivation the body's scope
/// table uses too. An expansion's blocks are read once, at the first call that needs them.
pub struct BodyBlocks<'a> {
    krate: blaze_id::CrateItemKey,
    nested: blaze_file_items::nested::Nested<'a>,
    hand: AVec<'a, Option<ItemKey>>,
    read: core::cell::RefCell<ReadExpansions<'a>>,
    alloc: Alloc<'a>,
}

/// The expansions a [`BodyBlocks`] has read, by index: each one's blocks and the place of its
/// text (its start and length) when read. The lowering appends expansions and never changes
/// one, so an entry stays true. A place that no longer matches means another lowering, and
/// every entry is read again.
struct ReadExpansions<'a> {
    made: AVec<'a, Option<super::scope_table::Expanded<'a>>>,
    at: AVec<'a, Option<(u32, u32)>>,
}

/// Why a call-site path in a block an expansion wrote is not read, while the names library
/// filed no module for that block. The block's call is no statement call of a block the
/// library filed: an expression call outside every block of the item, or a call whose writer
/// is unfiled too.
const UNFILED_BLOCK: &str = "the block declares items or holds statement calls, and the names library filed no module for it (its macro call is no statement call of a block the library filed), so a name it binds cannot be read";

/// The body's blocks for `text`, the whole text of the names library's item or member `body`
/// of the crate `krate`: the same scan of the same text the library made its block modules
/// from, so the same keys. A text whose scan does not end whole has none (`None`): its blocks
/// are not the library's either.
pub fn body_blocks<'a>(krate: blaze_id::CrateItemKey, body: ItemKey, text: &str, alloc: Alloc<'a>) -> Option<BodyBlocks<'a>> {
    let nested = blaze_file_items::nested::scan(text, alloc);
    if nested.end != blaze_file_items::nested::NestedEnd::Whole {
        return None;
    }
    let hand = blaze_scope::blocks::block_modules(krate, body, &nested, alloc);
    let read = ReadExpansions { made: Vec::new_in(alloc), at: Vec::new_in(alloc) };
    Some(BodyBlocks { krate, nested, hand, read: core::cell::RefCell::new(read), alloc })
}

/// The two questions [`ScopedBy`] asks of a body's blocks ([`BodyBlocks`]). It holds them
/// behind this trait because a [`BodyBlocks`] keeps the expansions it has read in a cell, so
/// its lifetime is invariant, and a `&'d BodyBlocks<'d>` of a body's local could not be
/// borrowed for as long as its own lifetime.
pub trait BlockModules {
    /// The module of the innermost block of the body's text holding the call written at
    /// `call` (item-relative) that the names library made a module. `None` when no such block
    /// holds the call.
    fn hand_module(&self, call: blaze_surface::Range) -> Option<ItemKey>;

    /// The module of the innermost block an expansion wrote that holds the body call at `call`
    /// (a range of `expansions.text`). `Ok(None)` when no such block holds it.
    fn expansion_module(&self, expansions: blaze_lower::Expansions<'_>, call: blaze_surface::Range) -> Result<Option<ItemKey>, &'static str>;
}

impl BlockModules for BodyBlocks<'_> {
    /// The module of the innermost block of the body's text holding the call written at
    /// `call` (item-relative) that the names library made a module: the last in text order of
    /// those that hold it, since a block inside another opens after it. `None` when no block
    /// holds the call.
    fn hand_module(&self, call: blaze_surface::Range) -> Option<ItemKey> {
        self.nested.blocks.iter().zip(self.hand.iter()).filter(|(b, m)| m.is_some() && b.open < call.start && call.end <= b.close).last().and_then(|(_, m)| *m)
    }

    /// The module of the innermost block an expansion wrote that holds the body call at `call`
    /// (a range of `expansions.text`), walking out from the call through each block around it
    /// and then each expansion's call. `Ok(None)` when the walk reaches the body's own text
    /// first: the written call's block is read ([`Self::hand_module`]).
    ///
    /// The Reference, "Statements", "Item declarations": "Declaring an item within a statement
    /// block restricts its scope to the block containing the statement." A block an expansion
    /// wrote is a block like any other, and item names are not hygienic ("Macros By Example",
    /// "Hygiene": only "loop labels, block labels, and local variables are looked up at the
    /// macro definition site"). So whichever text wrote a path, the block holding it is
    /// decided by where the path stands.
    ///
    /// A block that would hold a module and holds none is refused by name
    /// ([`UNFILED_BLOCK`]). A block the library did not write (no item, no call) binds
    /// nothing, and the walk goes on to the block around it.
    fn expansion_module(&self, expansions: blaze_lower::Expansions<'_>, call: blaze_surface::Range) -> Result<Option<ItemKey>, &'static str> {
        use blaze_lower::scope_origin::CallAt;
        let Some((mut e, mut at)) = expansions.holding(call.start) else { return Ok(None) };
        let mut read = self.read.try_borrow_mut().map_err(|_| "the body's expansion blocks were asked while they were being read")?;
        self.read_chain(&mut read, expansions, e)?;
        // Each step goes to an earlier expansion (a call is read before the expansion it
        // makes), so the walk takes at most one step per expansion.
        for _ in 0..=expansions.len() {
            let one = read.made.get(e as usize).and_then(|m| m.as_ref()).ok_or("an expansion of the chain was not read")?;
            let nested = &one.blocks.nested;
            let pos = at + blaze_scope::expanded_blocks::WRAP_OPEN;
            // The innermost block holding the call: the last in text order of those holding it.
            let mut b = nested.blocks.iter().rposition(|blk| blk.open < pos && pos < blk.close).map(|b| b as u32);
            // Block 0 stands for the calling block (expanded_blocks.rs `wrapped`), whose names
            // are the writer's, read after this loop.
            while let Some(i) = b.filter(|i| *i != 0) {
                if one.blocks.ordinals.get(i as usize).copied().flatten().is_some() {
                    match one.modules.get(i as usize).copied().flatten() {
                        Some(module) => return Ok(Some(module)),
                        None if binds(nested, i) => return Err(UNFILED_BLOCK),
                        None => {}
                    }
                }
                b = nested.blocks.get(i as usize).and_then(|blk| blk.parent);
            }
            match expansions.call_at(e).ok_or("an expansion's call is not in the body's text")? {
                CallAt::Item { .. } => return Ok(None),
                CallAt::Expansion { expansion, start } => {
                    e = expansion;
                    at = start;
                }
            }
        }
        Err("an expansion's chain of calls does not reach the body's text")
    }
}

impl<'a> BodyBlocks<'a> {
    /// These blocks as [`ScopedBy::blocks`] holds them.
    pub fn modules(&self) -> &dyn BlockModules {
        self
    }

    /// Reads the blocks of expansion `e` and of every expansion whose text holds a call of
    /// the chain to it, writers first, each once.
    fn read_chain(&self, read: &mut ReadExpansions<'a>, expansions: blaze_lower::Expansions<'_>, e: u32) -> Result<(), &'static str> {
        use blaze_lower::scope_origin::CallAt;
        let place = |j: u32| -> Option<(u32, u32)> { Some((*expansions.starts.get(j as usize)?, u32::try_from(expansions.text(j)?.len()).ok()?)) };
        // An entry read from another lowering's expansions: read all again.
        let stale = read.at.iter().enumerate().any(|(j, p)| p.is_some() && *p != place(j as u32));
        if stale {
            read.made.clear();
            read.at.clear();
        }
        let wanted = e as usize + 1;
        if read.made.len() < wanted {
            read.made.resize_with(wanted, || None);
            read.at.resize(wanted, None);
        }
        // The chain from `e` out to the body's text, innermost first.
        let mut chain: AVec<'_, u32> = Vec::new_in(self.alloc);
        let mut j = e;
        for _ in 0..=expansions.len() {
            if read.made.get(j as usize).is_some_and(|m| m.is_some()) {
                break;
            }
            chain.push(j);
            match expansions.call_at(j).ok_or("an expansion's call is not in the body's text")? {
                CallAt::Item { .. } => break,
                CallAt::Expansion { expansion, .. } => j = expansion,
            }
        }
        for &j in chain.iter().rev() {
            let one = super::scope_table::expanded(self.krate, &self.nested, Some(self.hand.as_slice()), &expansions, j, &read.made, self.alloc).map_err(|r| r.form())?;
            read.made[j as usize] = Some(one);
            read.at[j as usize] = place(j);
        }
        Ok(())
    }
}

/// Whether block `b` of an expansion's scan binds a name: an item declared in it (any kind,
/// `use` too), or a statement call, whose expansion may declare one. An expression call's
/// items are its own expansion's.
fn binds(nested: &blaze_file_items::nested::Nested<'_>, b: u32) -> bool {
    nested.items.iter().any(|i| i.block == b) || nested.calls.iter().any(|c| c.block == b && !c.expr)
}

/// A kept definition as this resolver answers it.
fn resolved<'d, 'a>(kept: Kept<'d>) -> Resolved<'d, 'a>
where
    'd: 'a,
{
    match kept {
        Kept::Prepared(prepared) => Resolved::Prepared(prepared),
        Kept::Written(def) => Resolved::Def(def.clone()),
    }
}

impl<'d, 'n: 'd, S: NamesSource<'n>> MacroNames<'d> for ScopedBy<'d, 'n, S> {
    fn resolve<'a>(&self, path: &str, at: PathAt, kind: ProcKind, alloc: Alloc<'a>) -> Result<Resolved<'d, 'a>, NoMacro<'a>>
    where
        'd: 'a,
    {
        let mut segments: AVec<'a, &str> = Vec::new_in(alloc);
        segments.extend(path.split(|c: char| c == ':' || c.is_whitespace()).filter(|s| !s.is_empty()));
        let absolute = path.trim_start().starts_with(':');
        let Some(&first) = segments.first() else { return Err(NoMacro::NotAPath) };
        if segments.iter().skip(1).any(|s| matches!(*s, "$crate" | "crate")) {
            return Err(NoMacro::NotAPath);
        }
        // Where the call's name is read, by its first token's mark: the one rule
        // (blaze-lower `mark_site::site_of`).
        let site = blaze_lower::mark_site::site_of(at.mark, first, absolute, self.module).map_err(NoMacro::Site)?;
        let (written_kind, rest) = match (first, site.dollar_crate) {
            _ if absolute => (PathKind::Abs, 0),
            (_, Some(krate)) => (PathKind::DollarCrate(krate), 1),
            (first, None) => match first {
            "crate" => (PathKind::Crate, 1),
            "self" => (PathKind::SelfMod, 1),
            "super" => {
                let supers = segments.iter().take_while(|s| **s == "super").count();
                (PathKind::Super(u8::try_from(supers).map_err(|_| NoMacro::NotAPath)?), supers)
            }
            _ => (PathKind::Plain, 0),
            },
        };
        let written = Written { kind: written_kind, segments: &segments[rest..] };
        let shape = match kind {
            ProcKind::FnLike => CallShape::FnLike,
            ProcKind::Attr => CallShape::Attr,
            ProcKind::Derive => CallShape::Derive,
        };
        // A path read at the call site (no opaque mark, no `$crate`) of a call written in the
        // body's text is read in the innermost written block around the call: an `extern
        // crate .. as` or `use` there binds its first segment (serde's derive writes
        // `extern crate serde as _serde; _serde::__require_serde_not_serde_core!();` in its
        // `const _` block). Every cell of that walk is the block module's (rule 21). Rust's
        // order is the walk's own (blaze-scope names.rs `lexical`): a one-segment name is read
        // first in textual scope, the block's `macro_rules!` and then each enclosing module's
        // (the body's module whole: a body's block has no `mod` line there), and only then as a
        // path from the block outward (its items, its `use super::*`, the preludes).
        // A call an expansion wrote inside a block of its own is read at that block first
        // (`PathAt::block`, `BlockModules::expansion_module`): proptest's `prop_assert!` writes
        // `extern crate alloc;` and `alloc::format!(..)` in one `if` block.
        let at_call = site.dollar_crate.is_none() && !matches!(at.mark, Some(m) if m.transparency == blaze_expand::Transparency::Opaque);
        let module = match (at.block, at.written) {
            (Some(block), _) if at_call => block,
            (None, Some(call)) if at_call => self.blocks.and_then(|b| b.hand_module(call)).unwrap_or(site.module),
            _ => site.module,
        };
        let found = self.names.macro_at(module, written).map_err(|e| NoMacro::Names(MacroDefError::Path(e)))?;
        let Some(src) = found.src else {
            // A procedural macro with no source row: its crate's runner, by the name it is
            // defined under. The last segment is that name unless a `use .. as` renamed it,
            // which the names do not record for a proc macro yet. The definition names the
            // crate by its RUNNER's key, as `definition_of` does (the runner knows a crate by
            // no other key); a crate no runner holds has no definition.
            let no_source = || NoMacro::Names(MacroDefError::NoSource { def: found.def, krate: found.krate });
            let runner = found.proc_key.ok_or_else(no_source)?;
            let name = segments.last().copied().unwrap_or(first);
            let def_at = DefAt {
                site: DefSite { krate: runner, module: None },
                edition: found.edition,
                export: Export::Exported,
                local_inner: LocalInner::No,
                transparency: None,
            };
            return MacroDefinition::proc_macro(runner, kind, name, &[], def_at, alloc).map(Resolved::Def).map_err(|_| no_source());
        };
        // A `macro_rules!` or `macro` is prepared once, by its defining item's key.
        if let (Some(prepared), MacroBody::Rules(_) | MacroBody::Macro2 { .. }) = (self.prepared, src.body) {
            if let Some(kept) = prepared.get(found.def.key) {
                return Ok(resolved(kept));
            }
            let def = definition_of(src, found.crate_key, found.proc_key, found.edition, shape, prepared.alloc()).map_err(|note| NoMacro::Names(MacroDefError::Refused(note)))?;
            return Ok(resolved(prepared.keep(found.def.key, def)));
        }
        let src: MacroSrc<'a> = src;
        definition_of(src, found.crate_key, found.proc_key, found.edition, shape, alloc).map(Resolved::Def).map_err(|note| NoMacro::Names(MacroDefError::Refused(note)))
    }

    /// The crate of the body's module.
    fn local_site(&self) -> Option<DefSite> {
        let krate = self.names.crate_of(self.module)?;
        Some(DefSite { krate: self.names.crate_key(krate)?, module: None })
    }

    /// [`BlockModules::expansion_module`]; with no blocks of the names library (a signature, a
    /// local item's own body), none.
    fn expansion_block(&self, expansions: blaze_lower::Expansions<'_>, call: blaze_surface::Range) -> Result<Option<ItemKey>, &'static str> {
        match self.blocks {
            Some(blocks) => blocks.expansion_module(expansions, call),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_id::{ContentId, CrateItemKey, ItemKey};
    use blaze_surface::Range;

    use blaze_scope::expanded_blocks::expansion_block_name;
    use blaze_scope::task::key_of;

    use super::{BlockModules, UNFILED_BLOCK, body_blocks};

    fn key(s: &[u8]) -> ContentId {
        ContentId::of("blaze-ladder scoped test", s)
    }

    /// The range of the `n`th occurrence of `s` in `text`.
    fn nth(text: &str, s: &str, n: usize) -> Range {
        let start = text.match_indices(s).nth(n).map(|(at, _)| at).unwrap_or_else(|| panic!("occurrence {n} of `{s}` in `{text}`"));
        let start = u32::try_from(start).unwrap_or(u32::MAX);
        Range::new(start, start + u32::try_from(s.len()).unwrap_or(u32::MAX))
    }

    /// A derive's `const _: () = { extern crate dep as _dep; _dep::m!(); impl .. };`: its block
    /// is written (a statement call, an impl), and a call inside it is read at the block's
    /// module, an inner block's call at the inner block's; a call outside every block at no
    /// block (the body's module: the control, the rule before blocks were read, for every
    /// call). A call in the impl's member `f` is in no block of this text: the scan does not
    /// read into an item (blaze-scope blocks.rs `a_block_with_an_impl_is_a_module`: the impl is
    /// copied whole), since `f`'s body is its own text, whose blocks are filed under `f`; the
    /// impl is an item of the constant's block, so that range answers the constant's block.
    /// (This test once asked that call to answer an inner module of this text: no module
    /// does, and none is wanted.)
    #[test]
    fn a_call_reads_its_innermost_written_block() {
        let text = "const _: () = { extern crate dep as _dep; _dep::m!(); impl T for U { fn f(&self) { { _dep::n!(); } } } { _dep::k!(); } };";
        let alloc = blaze_own::testing::heap().alloc();
        let krate = CrateItemKey(key(b"app"));
        let body = ItemKey(key(b"body"));
        let blocks = body_blocks(krate, body, text, alloc).unwrap_or_else(|| panic!("the text scans whole"));
        let nested = blaze_file_items::nested::scan(text, alloc);
        let modules = blaze_scope::blocks::block_modules(krate, body, &nested, alloc);
        let outer = modules.first().copied().flatten().unwrap_or_else(|| panic!("the constant's block is written: {modules:?}"));
        assert_eq!(blocks.hand_module(nth(text, "_dep::m!();", 0)), Some(outer), "the call in the constant's block");
        assert_eq!(blocks.hand_module(nth(text, "const _", 0)), None, "a range outside every block is read at no block");
        let inner = blocks.hand_module(nth(text, "_dep::k!();", 0)).unwrap_or_else(|| panic!("the inner call is in a written block: {modules:?}"));
        assert_ne!(inner, outer, "the inner call reads its own block's module, not the constant's");
        assert!(modules.contains(&Some(inner)), "the inner call reads a written block's module: {modules:?}");
        assert_eq!(blocks.hand_module(nth(text, "_dep::n!();", 0)), Some(outer), "a call in the impl's member is read at the block the impl is an item of");
    }

    /// The module the names library files for the `ordinal`th written block of the expansion
    /// of the call keyed `call` (scope_table.rs, "The identities").
    fn expansion_block(krate: CrateItemKey, call: ItemKey, ordinal: u32) -> ItemKey {
        let alloc = blaze_own::testing::heap().alloc();
        key_of(krate, call, "mod", expansion_block_name(call, ordinal, alloc).as_str(), 0)
    }

    /// proptest 1.11.0's `prop_assert!` (sugar.rs:754-757), reduced: a statement call whose
    /// expansion writes `if x { extern crate dep as __d ; let _m = __d :: unit ! ( ) ; }`. The
    /// Reference, "Statements": "Declaring an item within a statement block restricts its
    /// scope to the block containing the statement". So `__d::unit!` in the `if` block is read
    /// at the module the names library filed for that block, which holds the `extern crate`.
    /// Twin: the same expansion's `__d :: unit ! ( )` after the block is in no block the
    /// expansion wrote (`Ok(None)`: the written call's block, where no `__d` is bound, and the
    /// call is refused there). Control: without the expansion's block, the call would be read
    /// at the written call's block (`hand_module`), which is another module.
    #[test]
    fn a_call_an_expansion_wrote_in_its_own_block_reads_that_block() {
        let item = "fn f(x: bool) { check!(x); }";
        let expansion = "if x { extern crate dep as __d ; let _m = __d :: unit ! ( ) ; } __d :: unit ! ( ) ;";
        let alloc = blaze_own::testing::heap().alloc();
        let mut text = blaze_own::AString::new_in(alloc);
        text.push_str(item);
        text.push_str(expansion);
        let krate = CrateItemKey(key(b"app"));
        let body = ItemKey(key(b"body"));
        let blocks = body_blocks(krate, body, item, alloc).unwrap_or_else(|| panic!("the item scans whole"));
        let written = nth(item, "check!(x)", 0);
        let calls = [written];
        let expansions = blaze_lower::Expansions { text: text.as_str(), item_len: u32::try_from(item.len()).unwrap_or(u32::MAX), starts: &[0], calls: &calls };
        // The keys as the names library makes them: the fn's block, its statement call, the
        // call's `if` block (the expansion's first written block).
        let fn_block = blocks.hand_module(written).unwrap_or_else(|| panic!("the fn's block is written: it holds a statement call"));
        let call = key_of(krate, fn_block, "MacroCall", "", 0);
        let want = expansion_block(krate, call, 0);
        let inside = nth(text.as_str(), "__d :: unit ! ( )", 0);
        let after = nth(text.as_str(), "__d :: unit ! ( )", 1);
        assert_eq!(blocks.expansion_module(expansions, inside), Ok(Some(want)), "the path in the `if` block reads that block's module");
        assert_eq!(blocks.expansion_module(expansions, after), Ok(None), "the twin: the path after the block is in no block the expansion wrote");
        assert_ne!(Some(want), blocks.hand_module(written), "the control: the written call's block is another module");
        // Read again from the kept expansion: the same answer.
        assert_eq!(blocks.expansion_module(expansions, inside), Ok(Some(want)), "the second read answers the same");
    }

    /// Two expansions deep, as proptest's `prop_assert_eq!` calls `$crate::prop_assert!` inside
    /// `{{ let left = ..; .. }}`: the inner call is keyed under the module of the outer
    /// expansion's block that holds it, and the inner expansion's `if` block under that call.
    #[test]
    fn a_call_two_expansions_deep_reads_the_inner_expansions_block() {
        let item = "fn f(x: bool) { outer!(x); }";
        let first = "{ let left = x ; check ! ( left ) ; }";
        let second = "if left { extern crate dep as __d ; let _m = __d :: unit ! ( ) ; }";
        let mut text = std::string::String::from(item);
        text.push_str(first);
        text.push_str(second);
        let alloc = blaze_own::testing::heap().alloc();
        let krate = CrateItemKey(key(b"app"));
        let body = ItemKey(key(b"body"));
        let blocks = body_blocks(krate, body, item, alloc).unwrap_or_else(|| panic!("the item scans whole"));
        let written = nth(item, "outer!(x)", 0);
        let calls = [written, nth(&text, "check ! ( left )", 0)];
        let starts = [0, u32::try_from(first.len()).unwrap_or(u32::MAX)];
        let expansions = blaze_lower::Expansions { text: &text, item_len: u32::try_from(item.len()).unwrap_or(u32::MAX), starts: &starts, calls: &calls };
        let fn_block = blocks.hand_module(written).unwrap_or_else(|| panic!("the fn's block is written"));
        let outer = key_of(krate, fn_block, "MacroCall", "", 0);
        let first_block = expansion_block(krate, outer, 0);
        let check = key_of(krate, first_block, "MacroCall", "", 0);
        let want = expansion_block(krate, check, 0);
        assert_eq!(blocks.expansion_module(expansions, nth(&text, "__d :: unit ! ( )", 0)), Ok(Some(want)), "the path reads the inner expansion's `if` block");
        assert_eq!(blocks.expansion_module(expansions, calls[1]), Ok(Some(first_block)), "the inner call itself stands in the outer expansion's block");
    }

    /// A block an expansion wrote that declares an item, where the expansion's call is no
    /// statement call of a block the names library filed (a call outside every block of the
    /// item, a `const`'s initializer): refused by name, never read at an outer block, where
    /// the block's `__d` is not bound.
    #[test]
    fn an_unfiled_expansion_block_that_binds_is_refused() {
        let item = "const C: () = check!(x);";
        let expansion = "if x { extern crate dep as __d ; let _m = __d :: unit ! ( ) ; }";
        let mut text = std::string::String::from(item);
        text.push_str(expansion);
        let alloc = blaze_own::testing::heap().alloc();
        let blocks = body_blocks(CrateItemKey(key(b"app")), ItemKey(key(b"body")), item, alloc).unwrap_or_else(|| panic!("the item scans whole"));
        let calls = [nth(item, "check!(x)", 0)];
        let expansions = blaze_lower::Expansions { text: &text, item_len: u32::try_from(item.len()).unwrap_or(u32::MAX), starts: &[0], calls: &calls };
        assert_eq!(blocks.expansion_module(expansions, nth(&text, "__d :: unit ! ( )", 0)), Err(UNFILED_BLOCK));
    }

    /// `cfg_if!` (nybbles 0.4.8 `Nibbles::as_array`) expands through an attributed statement call
    /// `#[cfg(all(..))] $crate :: cfg_if ! { @ __temp_group .. }` whose `MACRO_CALL` range in
    /// body lowering starts at `$crate`, after the attribute, and whose expansion writes a
    /// `const { assert!(..); }` block. Both an attributed hand-written call and an attributed
    /// call inside an expansion must resolve to their filed block module, never `UNFILED_BLOCK`.
    #[test]
    fn an_attributed_statement_call_resolves_its_expansion_block() {
        let item = "fn as_array() { #[cfg(all())] outer!(); }";
        let first = "# [ cfg ( all ( ) ) ] $crate :: inner ! { }";
        let second = "const { assert ! ( true ) ; }";
        let alloc = blaze_own::testing::heap().alloc();
        let mut text = blaze_own::AString::new_in(alloc);
        text.push_str(item);
        text.push_str(first);
        text.push_str(second);
        let krate = CrateItemKey(key(b"app"));
        let body = ItemKey(key(b"body"));
        let blocks = body_blocks(krate, body, item, alloc).unwrap_or_else(|| panic!("the item scans whole"));
        // Body lowering places `MACRO_CALL` at the path (`outer!()`, `$crate :: inner ! { }`),
        // after any leading `#[cfg(..)]` attribute on the enclosing `MACRO_EXPR`.
        let written = nth(item, "outer!()", 0);
        let inner_call = nth(text.as_str(), "$crate :: inner ! { }", 0);
        let calls = [written, inner_call];
        let starts = [0, u32::try_from(first.len()).unwrap_or(u32::MAX)];
        let expansions = blaze_lower::Expansions { text: text.as_str(), item_len: u32::try_from(item.len()).unwrap_or(u32::MAX), starts: &starts, calls: &calls };
        let fn_block = blocks.hand_module(written).unwrap_or_else(|| panic!("the fn's block is written"));
        let outer = key_of(krate, fn_block, "MacroCall", "", 0);
        let inner = key_of(krate, outer, "MacroCall", "", 0);
        let want = expansion_block(krate, inner, 0);
        assert_eq!(blocks.expansion_module(expansions, nth(text.as_str(), "assert ! ( true )", 0)), Ok(Some(want)));
    }
}
