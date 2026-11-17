//! The world's answer for a body path that names nothing ([`blaze_typeck::PathMiss`]): which
//! written segment the names library stopped at, why, and what the segments before it named,
//! so the checker's E0425 names that segment (rule 19), not the path's last one.
//!
//! The Reference, "Paths", "Paths in expressions": a path is resolved segment by segment; the
//! library's [`PathError`] says at which segment of the path it was given (`at`) and why. The
//! path it was given may be longer than the written one (a local `use` or glob in front of it
//! stands for its own path): the index is taken back to the written segments here.

use blaze_id::ItemKey as LibKey;
use blaze_scope::names::{PathError, Why};
use blaze_typeck::{Def, Fetch, MissWhy, PathAnswer, PathMiss, PathPrefix};

use super::keys::checker_of;
use super::world_items::{BodyWorld, Gap};
use crate::resolve::{Kind, Ns, ResolveError};

impl<'n, S: super::source::NamesSource<'n>> BodyWorld<'_, 'n, S> {
    /// The miss for the library's refusal `e` of `rest` (read at `module`, from the crate's
    /// root when `abs`), where `rest[0]` is segment `lead` of the path as read and the written
    /// path's segment `i` is segment `i + shift` of it (`shift` > 0: a `use` or glob in front).
    pub(super) fn path_miss(&self, module: LibKey, dollar: Option<blaze_id::CrateItemKey>, abs: bool, rest: &[&str], lead: u32, shift: u32, e: PathError) -> Fetch<PathAnswer> {
        let why = match e.why {
            Why::NotFound => MissWhy::NotFound,
            Why::Private => MissWhy::Private,
            Why::Ambiguous => MissWhy::Ambiguous,
            Why::NotAModule => MissWhy::NotAModule,
            Why::SuperPastRoot => MissWhy::SuperPastRoot,
            // A macro call of the module was refused (its expansion may bind the name): a miss
            // whose words blaze-cli body_types.rs files as the body not checked under that call,
            // which the words name with why it was refused (path_miss_shown.rs).
            Why::Refused { call, why, .. } => MissWhy::Refused {
                call,
                why: match why {
                    blaze_surface::RefusedWhy::Expansion => blaze_typeck::Refusal::Expansion,
                    blaze_surface::RefusedWhy::UnknownEnv => blaze_typeck::Refusal::UnknownEnv,
                    blaze_surface::RefusedWhy::BuildScriptOutputs => blaze_typeck::Refusal::BuildScriptOutputs,
                },
            },
            // Not known: a scope the path could resolve in was not read whole (a crate not
            // read, a glob into one, the prelude), or a lookup through globs passed its bound.
            // "Not found" is a fact only when every such scope was read (rule 3: never an E0425
            // for a name that may exist); the body is not checked, and the note names the cause.
            Why::NotRead { .. } | Why::Unbounded => return self.gap(Gap::Resolve(ResolveError::Path(e))),
            // The library was given no segment: the path has none to name.
            Why::Empty => return Ok(Err(PathMiss::Whole)),
        };
        let Ok(at) = u32::try_from(e.at) else { return Ok(Err(PathMiss::Whole)) };
        let read_at = at + lead;
        // A step inside the `use`'s or glob's own path (before its binding, which is the written
        // path's first segment): the binding names no item.
        let Some(segment) = read_at.checked_sub(shift) else {
            return Ok(Err(PathMiss::At { segment: 0, after: None, why: MissWhy::ThroughUse }));
        };
        if at == 0 {
            return Ok(Err(PathMiss::At { segment, after: None, why }));
        }
        // What the segments before the failing one named: the same prefix, read again (only on
        // this error's path), in the type namespace a prefix is read in.
        let Some(prefix) = rest.get(..at as usize) else { return Ok(Err(PathMiss::At { segment, after: None, why })) };
        let target = match self.resolver().path_with(module, dollar, abs, prefix, Ns::Type) {
            Ok(t) => t,
            Err(err) => return self.gap(Gap::Resolve(err)),
        };
        let k = checker_of(target.key);
        let after = match target.kind {
            // A module reached through a written `std` that is bstd: the finding notes what
            // `std` named (std_is_bstd.rs, rule 19).
            Kind::Module => match self.std_is_bstd(module, rest, shift, why)? {
                Some(edition) => PathPrefix::Bstd { module: k, edition },
                None => PathPrefix::Module(k),
            },
            Kind::Struct | Kind::Enum | Kind::Union => PathPrefix::Def(Def::Adt(k)),
            Kind::Trait => PathPrefix::Def(Def::Trait(k)),
            _ => PathPrefix::Item(k),
        };
        Ok(Err(PathMiss::At { segment, after: Some(after), why }))
    }
}
