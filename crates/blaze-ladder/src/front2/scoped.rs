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
    /// The body's written blocks ([`written_blocks`]), in text order: a call-site path of a
    /// call written inside one is read at the innermost one's module, which declares the
    /// block's items (blaze-scope blocks.rs) and reaches the blocks around it and the module
    /// through its `use super::*`. Empty for a text with no block of the names library (a
    /// signature, a local item's own body).
    pub blocks: &'d [WrittenBlock],
}

/// A block of a body that the names library made a module (blaze-scope blocks.rs): where it is
/// in the body's text, and its module's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrittenBlock {
    /// The offset of its `{` in the body's text.
    pub open: u32,
    /// The offset after its `}`.
    pub close: u32,
    pub module: ItemKey,
}

/// The written blocks of `text`, the whole text of the names library's item or member `body`
/// of the crate `krate`, in text order: the same scan of the same text the library made its
/// block modules from, so the same keys (blaze-scope `blocks::block_modules`, as front2
/// locals.rs `Locals::keep` names them). A text whose scan does not end whole has none: its
/// blocks are not the library's either.
pub fn written_blocks<'a>(krate: blaze_id::CrateItemKey, body: ItemKey, text: &str, alloc: Alloc<'a>) -> AVec<'a, WrittenBlock> {
    let mut out: AVec<'a, WrittenBlock> = Vec::new_in(alloc);
    let nested = blaze_file_items::nested::scan(text, alloc);
    if nested.end != blaze_file_items::nested::NestedEnd::Whole {
        return out;
    }
    let modules = blaze_scope::blocks::block_modules(krate, body, &nested, alloc);
    for (block, module) in nested.blocks.iter().zip(modules.iter()) {
        if let Some(module) = *module {
            out.push(WrittenBlock { open: block.open, close: block.close, module });
        }
    }
    out
}

/// The module a call-site path of the call written at `call` (item-relative) is read at: the
/// innermost written block holding the call (the last in text order of those that hold it:
/// a block inside another opens after it), or the body's module when none does.
fn block_module(blocks: &[WrittenBlock], call: blaze_surface::Range, module: ItemKey) -> ItemKey {
    blocks.iter().filter(|b| b.open < call.start && call.end <= b.close).last().map_or(module, |b| b.module)
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
        let at_call = site.dollar_crate.is_none() && !matches!(at.mark, Some(m) if m.transparency == blaze_expand::Transparency::Opaque);
        let module = match at.written {
            Some(call) if at_call => block_module(self.blocks, call, site.module),
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
}

#[cfg(test)]
mod tests {
    use blaze_id::{ContentId, CrateItemKey, ItemKey};
    use blaze_surface::Range;

    use super::{block_module, written_blocks};

    /// A derive's `const _: () = { extern crate dep as _dep; _dep::m!(); impl .. };`: its block
    /// is written (a statement call, an impl), and a call inside it is read at the block's
    /// module, an inner block's call at the inner block's; a call outside every block at the
    /// body's module (the control: the rule before this fix, for every call).
    #[test]
    fn a_call_reads_its_innermost_written_block() {
        let text = "const _: () = { extern crate dep as _dep; _dep::m!(); impl T for U { fn f(&self) { { _dep::n!(); } } } };";
        let alloc = blaze_own::testing::heap().alloc();
        let key = |s: &[u8]| ContentId::of("blaze-ladder scoped test", s);
        let blocks = written_blocks(CrateItemKey(key(b"app")), ItemKey(key(b"body")), text, alloc);
        let module = ItemKey(key(b"module"));
        let at = |s: &str| {
            let start = u32::try_from(text.find(s).unwrap_or_else(|| panic!("`{s}` in the text"))).unwrap_or(u32::MAX);
            Range::new(start, start + u32::try_from(s.len()).unwrap_or(u32::MAX))
        };
        let outer = blocks.first().copied().unwrap_or_else(|| panic!("the constant's block is written: {blocks:?}"));
        assert_eq!(outer.open as usize, text.find('{').unwrap_or(0), "the first written block is the constant's: {blocks:?}");
        assert_eq!(block_module(&blocks, at("_dep::m!();"), module), outer.module, "the call in the constant's block: {blocks:?}");
        assert_ne!(outer.module, module, "a block's module is not the body's");
        assert_eq!(block_module(&blocks, at("const _"), module), module, "a range outside every block is read at the body's module");
        let inner = block_module(&blocks, at("_dep::n!();"), module);
        assert!(blocks.iter().any(|b| b.module == inner), "the inner call reads a written block's module: {blocks:?}");
    }
}
