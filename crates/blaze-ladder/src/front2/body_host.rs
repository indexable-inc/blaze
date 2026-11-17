//! The host a body's macro calls expand with (census 13, brief 13): the SAME answers the
//! crate's item-level expansion gets from blaze-scope's `Host` (resolve.rs), so a call means
//! one thing wherever it is written.
//!
//! - `env!`/`option_env!`: the crate's environment, `blaze_scope::crate_env::CrateEnv` (one per
//!   unit: the package's name and version, the crate's name, `CARGO_MANIFEST_DIR` as the
//!   package's stable `/package/N`, `OUT_DIR` and the `rustc-env` pairs of the joined build
//!   script record), through [`SourceTexts::crate_env`]. A name cargo sets whose value is not
//!   known, or a source with no environment, is refused (rule 3), never answered unset; a name
//!   the crate is compiled without is unset, and blaze-expand makes `env!` of it the
//!   Reference's compile error naming the variable (`Diagnostic::EnvNotDefined`).
//! - proc macros (tokio's `select!` calls tokio-macros' `select_priv_*`, `paste::paste!`): the
//!   sources' runner ([`SourceTexts::run_proc_macro`]), the one item expansion uses, in the
//!   crate's package and edition.
//! - `include!`/`include_str!`/`include_bytes!`: a file under the build script's `OUT_DIR` from
//!   its record (as blaze-build-script's `ScriptCx`), else the file the calling file's folder
//!   and the written path name (core's `include_str!`: "The file is located relative to the
//!   current file"), read from the crate's texts and bounded by its package (D60).
//! - `cfg!`: the crate's cfg atoms (a fn-like call in a body is answered by the body's own set
//!   first: blaze-lower's `BodyCx`).
//! - `file!()`/`module_path!()` in eager input (`concat!(file!(), ..)`): the file's STABLE name
//!   (`SourceFile::shown`, past the package's folder: never a machine path) and the module's
//!   path; each answer is recorded by the expander in the call's read list (`ReadKind::File`,
//!   `ReadKind::ModulePath`), which joins the body's (`ByName::reads`). A plain `line!()`,
//!   `column!()` or `file!()` never asks: it is a hole the expansion keeps, filled where the
//!   item's place is known (rule 71). `line!()`/`column!()` in eager input are refused by name
//!   ([`HostAsk::Place`]): `ExpandCx::place` is not told which call asks, so no item-relative
//!   position can be answered (the change wanted is in the lane's report).
//!
//! Every answer is a function of the crate, the body's file and the question: no machine path,
//! no ambient environment (rule 75).

use blaze_expand::seen::SeenRoot;
use blaze_expand::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream};
use blaze_id::ItemKey as LibKey;
use blaze_own::Alloc;
use blaze_scope::resolve::{At, SourceTexts};

use super::heads::CrateCfg;
use super::source::NamesSource;

fn refused(ask: HostAsk) -> HostError {
    HostError { ask, why: HostWhy::Refused }
}

/// A run's sources as a body's expansion reads them, by crate index: an object with no
/// lifetime parameter of its own, so a [`super::heads::Store`] can hold it
/// ([`super::heads::Store::sources`]) without tying its own lifetime to the sources'.
/// [`Sourced`] gives it for any [`SourceTexts`].
pub trait BodySources {
    /// The text of the file at `path` (absolute, under the crate's package folder).
    fn read(&self, krate: usize, path: &str) -> Option<&str>;
    /// The variable `name` of the crate's environment: `Ok(None)` when it is unset; refused
    /// when the source has no environment for the crate or cargo sets it to a value not known.
    fn env(&self, krate: usize, name: &str) -> Result<Option<&str>, HostError>;
    /// The crate's package folder with its trailing `/` (`/package/N/`): an include's bound.
    fn folder(&self, krate: usize) -> Option<&str>;
    /// The root the crate sees (D96: `/package`, `/workspace/<dir>`): an absolute include path
    /// under it is the package's file. `None`: no environment, and such a path is outside.
    fn seen(&self, krate: usize) -> Option<SeenRoot<'_>> {
        let _ = krate;
        None
    }
    /// The file at `path` when `path` is under the crate's build script's `OUT_DIR`: `Some` of
    /// the record's bytes or `Some(None)` when the record has no such file; `None` when the
    /// path is not under it (or the crate has no joined record).
    fn script_file(&self, krate: usize, path: &str) -> Option<Option<&[u8]>>;
    /// Runs the proc macro `def` on `call`, written in the crate at `krate`.
    fn run_proc_macro<'b>(&self, krate: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError>;
}

/// A [`SourceTexts`] as a body's [`BodySources`]: the same texts, environment and runner
/// item-level expansion reads.
pub struct Sourced<'s, S>(pub &'s S);

impl<'s, 'a: 's, S: SourceTexts<'a>> BodySources for Sourced<'s, S> {
    fn read(&self, krate: usize, path: &str) -> Option<&str> {
        self.0.read(krate, path)
    }

    fn env(&self, krate: usize, name: &str) -> Result<Option<&str>, HostError> {
        let env = self.0.crate_env(krate).ok_or(refused(HostAsk::Env))?;
        env.get(name).map_err(|_| refused(HostAsk::Env))
    }

    fn folder(&self, krate: usize) -> Option<&str> {
        self.0.crate_env(krate).map(|env| env.read_folder())
    }

    fn seen(&self, krate: usize) -> Option<SeenRoot<'_>> {
        self.0.crate_env(krate).map(|env| env.seen())
    }

    fn script_file(&self, krate: usize, path: &str) -> Option<Option<&[u8]>> {
        let script = self.0.crate_env(krate)?.script()?;
        script.file_at(path)
    }

    fn run_proc_macro<'b>(&self, krate: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        self.0.run_proc_macro(krate, def, call, alloc)
    }
}

/// The body's file: its path (absolute, under the package) and the path `file!()` gives.
#[derive(Clone, Copy)]
struct BodyFile<'h> {
    path: &'h str,
    shown: &'h str,
}

/// The expansion host of one body: see the module's doc.
pub struct BodyHost<'h> {
    sources: Option<&'h dyn BodySources>,
    krate: usize,
    /// `None`: the body's text is in a macro's expansion, or the names are records (no file
    /// table): its includes and `file!()` are refused by name.
    file: Option<BodyFile<'h>>,
    module_path: Option<&'h str>,
    atoms: &'h [(&'h str, Option<&'h str>)],
    alloc: Alloc<'h>,
}

impl<'h> BodyHost<'h> {
    /// The host of the body of `item` (a fn, or a member's container), written in `module` of
    /// the crate at `krate`; `sources` is the run's ([`super::heads::Store::sources`]): `None`
    /// refuses every env read, include and proc macro by name.
    pub fn of<S: NamesSource<'h>>(names: S, sources: Option<&'h dyn BodySources>, krate: usize, item: LibKey, module: LibKey, alloc: Alloc<'h>) -> BodyHost<'h> {
        let file = match (names.solved(), names.place_of(item)) {
            (Some(solved), Some(place)) if place.krate == krate => match place.at {
                At::File(i) => solved.krate(krate).files.get(i as usize).map(|f| BodyFile { path: f.path, shown: f.shown }),
                At::Expansion(_) => None,
            },
            _ => None,
        };
        BodyHost { sources, krate, file, module_path: names.module_path(module), atoms: names.cfg_atoms(krate).unwrap_or(&[]), alloc }
    }
}

impl ExpandCx for BodyHost<'_> {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        self.sources.ok_or(refused(HostAsk::Env))?.env(self.krate, name)
    }

    /// The crate's cfg atoms: a predicate is true exactly when the crate is compiled with it.
    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        blaze_lower::cfg_outcome(predicate, &mut CrateCfg(self.atoms), self.alloc)
    }

    fn include(&self, written: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        let sources = self.sources.ok_or(refused(HostAsk::Include))?;
        if let Some(file) = sources.script_file(self.krate, written) {
            return Ok(file.map_or(Included::Missing, Included::Bytes));
        }
        let file = self.file.ok_or(refused(HostAsk::Include))?;
        let package = sources.folder(self.krate).ok_or(refused(HostAsk::Include))?;
        // The one place an include's path is made, the item host's too (D96 (4): an absolute
        // path under the root the crate sees is its package's).
        let Some(path) = blaze_scope::included::included_file(package, sources.seen(self.krate), file.path, written, self.alloc) else {
            return Ok(Included::Outside);
        };
        Ok(sources.read(self.krate, path.as_str()).map_or(Included::Missing, |text| Included::Bytes(text.as_bytes())))
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        match what {
            PlaceOf::File => self.file.map(|f| Place::Text(f.shown)).ok_or(refused(HostAsk::Place)),
            PlaceOf::ModulePath => self.module_path.map(Place::Text).ok_or(refused(HostAsk::Place)),
            // Not told which call asks: no item-relative position to answer (module doc).
            PlaceOf::Line | PlaceOf::Column => Err(refused(HostAsk::Place)),
        }
    }

    fn run_proc_macro<'b>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        self.sources.ok_or(refused(HostAsk::ProcMacro))?.run_proc_macro(self.krate, def, call, alloc)
    }
}

#[cfg(test)]
mod tests {
    use blaze_expand::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced};
    use blaze_own::Alloc;

    use super::{BodyFile, BodyHost, BodySources};

    /// One crate (index 0) of package `/package/3/`: `CARGO_PKG_NAME` is `tempo-node`,
    /// `CARGO_PKG_AUTHORS` is cargo's but not known, and one file besides the body's.
    struct One;

    impl BodySources for One {
        fn read(&self, krate: usize, path: &str) -> Option<&str> {
            (krate == 0 && path == "/package/3/src/abi.json").then_some("[]")
        }
        fn env(&self, krate: usize, name: &str) -> Result<Option<&str>, HostError> {
            match (krate, name) {
                (0, "CARGO_PKG_NAME") => Ok(Some("tempo-node")),
                (0, "CARGO_PKG_AUTHORS") => Err(HostError { ask: HostAsk::Env, why: HostWhy::Refused }),
                _ => Ok(None),
            }
        }
        fn folder(&self, krate: usize) -> Option<&str> {
            (krate == 0).then_some("/package/3/")
        }
        fn seen(&self, krate: usize) -> Option<blaze_expand::seen::SeenRoot<'_>> {
            (krate == 0).then_some(blaze_expand::seen::SeenRoot::Package)
        }
        fn script_file(&self, _: usize, _: &str) -> Option<Option<&[u8]>> {
            None
        }
        fn run_proc_macro<'b>(&self, _: usize, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'b>) -> Result<Produced<'b>, HostError> {
            Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed })
        }
    }

    fn host(sources: Option<&'static dyn BodySources>) -> BodyHost<'static> {
        BodyHost {
            sources,
            krate: 0,
            file: Some(BodyFile { path: "/package/3/src/lib.rs", shown: "src/lib.rs" }),
            module_path: Some("tempo_node::rpc"),
            atoms: &[("unix", None)],
            alloc: Alloc::global(),
        }
    }

    /// CLEAN: a body's `env!("CARGO_PKG_NAME")` reads the crate's environment (was refused by
    /// `NoHost`: tempo census 13, `34 Macro ::core::env`); `file!()` in eager input is the
    /// file's stable name, never a machine path; an include reads under the package.
    /// core's `env!` doc (pinned library, `macros/mod.rs`): "Inspects an environment variable
    /// at compile time. This macro will expand to the value of the named environment variable
    /// at compile time, yielding an expression of type `&'static str`." Prints nothing; passes.
    #[test]
    fn a_body_s_env_reads_the_crate_s_environment_and_its_file_is_the_stable_name() {
        static SOURCES: One = One;
        let h = host(Some(&SOURCES));
        assert_eq!(h.env("CARGO_PKG_NAME"), Ok(Some("tempo-node")));
        assert_eq!(h.place(PlaceOf::File), Ok(Place::Text("src/lib.rs")));
        assert_eq!(h.place(PlaceOf::ModulePath), Ok(Place::Text("tempo_node::rpc")));
        assert_eq!(h.include("abi.json", IncludeAs::Str), Ok(Included::Bytes(&b"[]"[..])));
        assert_eq!(h.include("../../outside.rs", IncludeAs::Str), Ok(Included::Outside));
        // D96 (4): `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/abi.json"))` reads the
        // package's file; any other absolute path names the machine and stays outside.
        assert_eq!(h.include("/package/src/abi.json", IncludeAs::Str), Ok(Included::Bytes(&b"[]"[..])));
        assert_eq!(h.include("/etc/abi.json", IncludeAs::Str), Ok(Included::Outside));
    }

    /// REFUSED: a name the crate is compiled without is unset, which blaze-expand makes the
    /// Reference's error ("If the environment variable is not defined, then a compilation
    /// error will be emitted": core's `env!` doc); a name cargo sets to a value not known, a
    /// host with no sources, and `line!()` in eager input (no call position given) are
    /// refused by name, never answered. Prints nothing; passes.
    #[test]
    fn an_unset_name_is_none_and_an_unknown_one_is_refused() {
        static SOURCES: One = One;
        let h = host(Some(&SOURCES));
        let env_refused = HostError { ask: HostAsk::Env, why: HostWhy::Refused };
        assert_eq!(h.env("TEMPO_NOT_SET"), Ok(None));
        assert_eq!(h.env("CARGO_PKG_AUTHORS"), Err(env_refused));
        assert_eq!(host(None).env("CARGO_PKG_NAME"), Err(env_refused));
        assert_eq!(h.place(PlaceOf::Line), Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused }));
    }
}
