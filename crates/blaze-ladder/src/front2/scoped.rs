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
        let module = site.module;
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
