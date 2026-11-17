//! A body's macro calls resolved by the names library: [`Scoped`] is the [`MacroNames`] of
//! one module, which a [`crate::by_name::ByName`] made with `with_names` expands through. A
//! call's path becomes a [`Written`] path and goes to blaze-scope's
//! `Names::macro_definition`, the one place a macro's source becomes a definition (the names
//! fill builds its own there too, so an item and a body never disagree on what a path names).
//!
//! Where a path is resolved (the Reference, "Macros By Example", Hygiene: "By default, all
//! identifiers referred to in a macro expansion are looked up as if they appeared at the
//! invocation site", with `$crate` the exception, "the crate where the macro was defined";
//! and a `macro` is def-site, its names resolved where it is written):
//! - a path the body writes: in the body's module, after the macros the body defines before
//!   it (textual scope wins over path-based scope);
//! - a path an expansion wrote: `$crate::m` in the crate the expansion's mark names (the
//!   definition's); a path under an opaque mark (a `macro`'s) in the definition's module;
//!   every other path at the call site, the body's module, as a written one.
//!
//! A procedural macro (the names give no source for it) becomes blaze-expand's proc-macro
//! definition, which the host's `ExpandCx::run_proc_macro` runs.
//!
//! A `macro_rules!` or `macro` is lexed and compiled (blaze-expand's `prepare`) once per
//! build, not at each call: [`Prepares`] holds each by the key of the item that defines it,
//! found by the path's lookup before any work (one entry per distinct macro called).

use blaze_expand::{DefAt, DefSite, Export, LocalInner, MacroDefinition, ProcKind};
use blaze_own::{AVec, Alloc, Vec};
use blaze_scope::names::Written;
// The one home of the prepared definitions, shared with the item-position expansions.
pub use blaze_scope::prepares::Prepares;
use blaze_scope::prepares::Kept;
use blaze_scope::resolve::{ItemAt, MacroDefError, Names};
use blaze_scope::task::{MacroBody, MacroSrc};
use blaze_scope::{CallShape, Namespace, PathKind};
use blaze_surface::ItemKey;

use crate::by_name::{MacroNames, NoMacro, PathAt, Resolved};

/// The macro names of one module: see the module's doc.
pub struct Scoped<'d, 'p: 'd> {
    /// The names, and every definition built from them, live for `'p` (the build's); this
    /// body's resolver is borrowed for `'d`.
    pub names: &'d Names<'p>,
    /// The module the body is in (blaze-scope's `Names::module_of` of its item). The
    /// `macro_rules!` the body itself writes are in textual scope before any of these names:
    /// the driver hands them to each call ([`crate::driver::BodyCall::locals`]).
    pub module: ItemKey,
    /// The definitions prepared so far, shared by every body's `Scoped` over `names`; `None`
    /// prepares a definition at each call (a caller with no place to keep them across bodies).
    pub prepared: Option<&'d Prepares<'p>>,
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

impl<'d, 'p: 'd> MacroNames<'d> for Scoped<'d, 'p> {
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
        // (`crate::mark_site::site_of`).
        let site = crate::mark_site::site_of(at.mark, first, absolute, self.module).map_err(NoMacro::Site)?;
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
        // The defining item first: a `macro_rules!` or `macro` is prepared once, by its key.
        let found = self.prepared.map(|prepared| (prepared, self.names.item_at(module, Namespace::Macros, written, alloc)));
        let rules_item = match found {
            Some((prepared, Ok(ItemAt { def, macro_src: Some(MacroSrc { body: MacroBody::Rules(_) | MacroBody::Macro2 { .. }, .. }), .. }))) => {
                if let Some(kept) = prepared.get(def.key) {
                    return Ok(resolved(kept));
                }
                Some((prepared, def.key))
            }
            _ => None,
        };
        let names: &Names<'p> = self.names;
        let built = match rules_item {
            Some((prepared, item)) => names.macro_definition(module, shape, written, prepared.alloc()).map(|def| resolved(prepared.keep(item, def))),
            None => {
                let names: &Names<'a> = names;
                names.macro_definition(module, shape, written, alloc).map(Resolved::Def)
            }
        };
        match built {
            Ok(resolved) => Ok(resolved),
            Err(MacroDefError::NoSource { def, krate }) => {
                // A procedural macro: its crate's runner, by the name it is defined under. The
                // last segment is that name unless a `use .. as` renamed it, which the names do
                // not record for a proc macro yet. The definition names the crate by its
                // RUNNER's key, as blaze-scope's `definition_of` does (the runner knows a crate
                // by no other key); a crate no runner holds has no definition.
                let name = segments.last().copied().unwrap_or(first);
                let owner = names.krate(krate);
                let Some(runner) = owner.proc_key else { return Err(NoMacro::Names(MacroDefError::NoSource { def, krate })) };
                let at = DefAt {
                    site: DefSite { krate: runner, module: None },
                    edition: owner.edition,
                    export: Export::Exported,
                    local_inner: LocalInner::No,
                    transparency: None,
                };
                MacroDefinition::proc_macro(runner, kind, name, &[], at, alloc)
                    .map(Resolved::Def)
                    .map_err(|_| NoMacro::Names(MacroDefError::NoSource { def, krate }))
            }
            Err(e) => Err(NoMacro::Names(e)),
        }
    }

    /// The crate of the body's module.
    fn local_site(&self) -> Option<DefSite> {
        let krate = self.names.crate_of(self.module)?;
        Some(DefSite { krate: self.names.krate(krate).key, module: None })
    }
}
