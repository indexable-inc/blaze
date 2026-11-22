//! A crate's expansion host with its build script's outputs in view: `env!("OUT_DIR")` is the
//! sandbox's `OUT_DIR` the script ran with (kept in the record), `env!` of a `cargo:rustc-env`
//! pair is its value, and `include!`/`include_str!`/`include_bytes!` of a path under that
//! `OUT_DIR` is the record's file, however the crate came by the path (`concat!` of
//! `env!("OUT_DIR")`, or a path the script printed into a `rustc-env` value).
//! Everything else is the wrapped host's. The expander records each answer as a tracked read
//! (`ReadKind::Env`, `IncludeStr`, `IncludeBytes` with the value's digest), so a changed output
//! invalidates exactly the items that read it; no machine path enters a key (rule 25).

use crate::outputs::OutputsRef;
use blaze_expand::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream};
use blaze_own::Alloc;

/// `inner` with the outputs of the crate's build script `out` laid over it, read in place from
/// the script's record.
pub struct ScriptCx<'o, C> {
    pub inner: C,
    pub out: OutputsRef<'o>,
}

impl<C: ExpandCx> ExpandCx for ScriptCx<'_, C> {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        if name == "OUT_DIR" {
            return Ok(Some(self.out.out_dir()));
        }
        // A later pair of one name is the one cargo gives the crate.
        match self.out.env().filter(|(key, _)| *key == name).last() {
            Some((_, value)) => Ok(Some(value)),
            None => self.inner.env(name),
        }
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        self.inner.cfg(predicate)
    }

    fn include(&self, path: &str, how: IncludeAs) -> Result<Included<'_>, HostError> {
        let Some(file) = self.out.file_at(path) else {
            return self.inner.include(path, how);
        };
        // A stored file (a native script's `OUT_DIR` file) is read from the record's file area,
        // this one file only; one it cannot read is an unreadable include, never no file.
        Ok(match file {
            Some(file) => Included::Bytes(self.out.bytes_of(file).map_err(|_| HostError { ask: HostAsk::Include, why: HostWhy::Unreadable })?),
            None => Included::Missing,
        })
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        self.inner.place(what)
    }

    // A wrapper answers what its inner host answers, for a told call too (the default would
    // ask the inner `place` and lose the call).
    fn place_of_call(&self, what: PlaceOf, call: u32) -> Result<Place<'_>, HostError> {
        self.inner.place_of_call(what, call)
    }

    fn run_proc_macro<'a>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        self.inner.run_proc_macro(def, call, alloc)
    }
}
