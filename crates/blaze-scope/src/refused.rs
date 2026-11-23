//! A macro call whose expansion was REFUSED (an `include!(concat!(env!("OUT_DIR"), ..))` of a
//! package whose build script has no outputs yet, a definition not held, a lex error, ..): it
//! is stored as refused, never as an empty expansion (rules 3 and 19). An empty expansion would
//! make every name the call might have written a plain "not found" in each dependent, naming
//! nothing; a refused one is written into its calling module's scope as
//! [`ScopeError::Refused`], and a path that looks a name up in that module and finds none
//! answers [`crate::names::Why::Refused`]: unknown, because that call was refused, and why.
//!
//! The reason is typed ([`RefusedWhy`]): the one cause a dependent can act on by name is the
//! build script's (`OUT_DIR`, which cargo sets for a package with a build script: "the folder
//! in which all output and intermediate artifacts should be placed", the cargo book,
//! "Environment variables Cargo sets for build scripts"); an `env!` of another variable this
//! build could not answer names the variable; any other refusal names the macro, the crate's
//! notes (`NamesNote::Expand`) holding the whole reason.

use core::cell::Cell;

use blaze_expand::{ExpandCx, HostAsk, HostError, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream};
use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Bump};
use blaze_surface::{RefusedWhy, ScopeError};

use crate::resolve::Expansion;
use crate::scope::Resolved;

/// Why a call's expansion was refused, and where it stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refused<'a> {
    /// The module the call is written in (`None`: the call is no call the lowering knows, so
    /// it stands in no module; its note names it).
    pub module: Option<ItemKey>,
    /// [`RefusedWhy::BuildScriptOutputs`] and [`RefusedWhy::UnknownEnv`]: the variable's name;
    /// [`RefusedWhy::Expansion`]: the macro's path as written.
    pub name: &'a str,
    pub why: RefusedWhy,
}

impl<'a> Refused<'a> {
    /// The refusal of a call written in `module` as the macro path `macro_name` (`None`: no call
    /// the lowering knows), whose expansion's first refused `env!` read was `env`.
    /// `out_dir_unknown`: the crate's environment holds `OUT_DIR` as set by cargo but not known
    /// here (its package has a build script and no outputs record is joined:
    /// `crate::crate_env::CrateEnv::of`).
    pub(crate) fn of(src: Option<(ItemKey, &'a str)>, env: Option<&'a str>, out_dir_unknown: bool) -> Refused<'a> {
        let module = src.map(|(module, _)| module);
        match env {
            Some(name) if name == "OUT_DIR" && out_dir_unknown => Refused { module, name, why: RefusedWhy::BuildScriptOutputs },
            Some(name) => Refused { module, name, why: RefusedWhy::UnknownEnv },
            None => Refused { module, name: src.map_or("", |(_, name)| name), why: RefusedWhy::Expansion },
        }
    }
}

/// The first variable whose `env!` (or `option_env!`) read the host refused while one call was
/// expanded: a refused read stops the expansion (blaze-expand's builtins pass the host's error
/// on), so it is that expansion's cause.
pub(crate) struct EnvRefusal<'a> {
    first: Cell<Option<&'a str>>,
    alloc: Bump<'a>,
}

impl<'a> EnvRefusal<'a> {
    pub(crate) fn new_in(alloc: Bump<'a>) -> EnvRefusal<'a> {
        EnvRefusal { first: Cell::new(None), alloc }
    }

    fn note(&self, name: &str) {
        if self.first.get().is_none() {
            let kept: &'a str = self.alloc.alloc_str(name);
            self.first.set(Some(kept));
        }
    }

    pub(crate) fn first(&self) -> Option<&'a str> {
        self.first.get()
    }
}

/// `inner`, with each refused `env!` read noted in `env`; every answer is `inner`'s, unchanged.
pub(crate) struct Watched<'c, 'a, C> {
    pub(crate) inner: &'c C,
    pub(crate) env: &'c EnvRefusal<'a>,
}

impl<C: ExpandCx> ExpandCx for Watched<'_, '_, C> {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        let answer = self.inner.env(name);
        if let Err(HostError { ask: HostAsk::Env, .. }) = answer {
            self.env.note(name);
        }
        answer
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        self.inner.cfg(predicate)
    }

    fn include(&self, path: &str, how: IncludeAs) -> Result<Included<'_>, HostError> {
        self.inner.include(path, how)
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        self.inner.place(what)
    }

    // A wrapper answers what its inner host answers, for a told call too (the default would
    // ask the inner `place` and lose the call).
    fn place_of_call(&self, what: PlaceOf, call: u32) -> Result<Place<'_>, HostError> {
        self.inner.place_of_call(what, call)
    }

    fn run_proc_macro<'b>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        self.inner.run_proc_macro(def, call, alloc)
    }
}

/// Writes each refused call of `expansions` into its calling module's scope in `done`, as
/// [`ScopeError::Refused`], in the order of the calls' keys (deterministic: a map's order never
/// reaches the scope). A call whose module has no scope here (no call the lowering knows) is
/// written nowhere: its `NamesNote::Expand` names it.
pub(crate) fn write_refused<'a>(
    done: &mut HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    expansions: &HashMap<ItemKey, Expansion<'a>, Alloc<'a>>,
    alloc: Alloc<'a>,
) {
    let mut refused: AVec<'a, (ItemKey, Refused<'a>)> = AVec::new_in(alloc);
    refused.extend(expansions.iter().filter_map(|(call, e)| e.refused.map(|r| (*call, r))));
    refused.sort_unstable_by_key(|(call, _)| *call);
    for (call, r) in refused.iter() {
        let Some(module) = r.module else { continue };
        if let Some(scope) = done.get_mut(&module) {
            scope.scope.errors.push(ScopeError::Refused { call: *call, name: r.name, why: r.why });
        }
    }
}
