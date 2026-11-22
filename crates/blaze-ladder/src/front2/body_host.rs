//! The host a body's macro calls expand with (census 13, brief 13): the SAME answers the
//! crate's item-level expansion gets from blaze-scope's `Host` (resolve.rs), so a call means
//! one thing wherever it is written.
//!
//! - `env!`/`option_env!`: the crate's environment, `blaze_scope::crate_env::CrateEnv` (one per
//!   unit: the package's name and version, the crate's name, `CARGO_MANIFEST_DIR` as the
//!   package's stable `/package/N`, `OUT_DIR` and the `rustc-env` pairs of the joined build
//!   script record), through [`SourceTexts::crate_env`]: in `blaze check`, the owner crate's
//!   environment made by `CrateEnv::of` from the crate list's facts of it (blaze-cli
//!   body_types.rs `owner_env`), the one construction the item host's comes from. A name cargo sets whose value is not
//!   known, or a source with no environment, is refused (rule 3), never answered unset; a name
//!   the crate is compiled without is unset, and blaze-expand makes `env!` of it the
//!   Reference's compile error naming the variable (`Diagnostic::EnvNotDefined`).
//! - proc macros (tokio's `select!` calls tokio-macros' `select_priv_*`, `paste::paste!`): the
//!   sources' runner ([`SourceTexts::run_proc_macro`]), the one item expansion uses, in the
//!   crate's package and edition.
//! - `include!`/`include_str!`/`include_bytes!`: a file under the build script's `OUT_DIR` from
//!   its record (as blaze-build-script's `ScriptCx`), else the file the calling file's folder
//!   and the written path name (core's `include_str!`: "The file is located relative to the
//!   current file"). Data includes read bytes under the same source root as the item host.
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

use core::cell::LazyCell;

use blaze_expand::seen::SeenRoot;
use blaze_expand::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream};
use blaze_id::ItemKey as LibKey;
use blaze_own::{AString, Alloc};
use blaze_scope::resolve::{At, SourceTexts};

use super::heads::CrateCfg;
use super::source::{FilePaths, NamesSource};

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
    /// Bytes of a data include at `path`, with the calling file for the job's read list.
    fn read_data(&self, krate: usize, from: &str, path: &str) -> Option<&[u8]>;
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
    /// What `file!()` gives for the file at `path` (package-relative) in the crate: the item
    /// host's answer (`blaze_scope::crate_env::CrateEnv::shown_file`, D96 (4)). `None`: no
    /// environment naming the package, and the read is refused.
    fn shown_file<'b>(&self, krate: usize, path: &str, alloc: Alloc<'b>) -> Option<AString<'b>> {
        let _ = (krate, path, alloc);
        None
    }
    /// The file at `path` when `path` is under the crate's build script's `OUT_DIR`: `Some` of
    /// the record's bytes or `Some(Ok(None))` when the record has no such file, `Some(Err)`
    /// when it names a stored file whose bytes could not be read; `None` when the path is not
    /// under it (or the crate has no joined record).
    fn script_file(&self, krate: usize, path: &str) -> Option<Result<Option<&[u8]>, HostError>>;
    /// Runs the proc macro `def` on `call`, written in the crate at `krate`.
    fn run_proc_macro<'b>(&self, krate: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError>;
    /// Decision D111: files, as findings at `call`, the reads of `reads` in `span` a member's
    /// proc macro made outside its own dir, through blaze-scope's one filing
    /// (`blaze_scope::left_reads::file_left_reads`), the item host's own; returns how many.
    /// Required, never defaulted: no body source can leave the finding out (super::body_left).
    fn file_left_reads(&self, krate: usize, call: blaze_scope::left_reads::CallAt<'_>, reads: &blaze_expand::Reads<'_>, span: core::ops::Range<usize>, alloc: Alloc<'_>) -> u32;
    /// Decision D111: an include a body's call made whose file lies outside its member's own
    /// package and inside the workspace, told as `left` to the item host's one sink
    /// (`SourceTexts::left_package`: a warning at the call). Required, never defaulted: no body
    /// source can leave the finding out (super::body_left `file_body_left_includes`).
    fn left_package(&self, krate: usize, left: blaze_scope::include_place::LeftPackage<'_>);
}

/// A [`SourceTexts`] as a body's [`BodySources`]: the same texts, environment and runner
/// item-level expansion reads.
pub struct Sourced<'s, S>(pub &'s S);

impl<'s, 'a: 's, S: SourceTexts<'a>> BodySources for Sourced<'s, S> {
    fn read(&self, krate: usize, path: &str) -> Option<&str> {
        self.0.read(krate, path)
    }

    fn read_data(&self, krate: usize, from: &str, path: &str) -> Option<&[u8]> {
        self.0.read_data(krate, from, path)
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

    fn shown_file<'b>(&self, krate: usize, path: &str, alloc: Alloc<'b>) -> Option<AString<'b>> {
        self.0.crate_env(krate)?.shown_file(path, alloc)
    }

    fn script_file(&self, krate: usize, path: &str) -> Option<Result<Option<&[u8]>, HostError>> {
        let script = self.0.crate_env(krate)?.script()?;
        // A stored file (a native script's `OUT_DIR` file) is read from the record's file
        // area, this one file only; one it cannot read is an unreadable include, never no file.
        let unreadable = |_| HostError { ask: HostAsk::Include, why: HostWhy::Unreadable };
        script.file_at(path).map(|file| file.map(|file| script.bytes_of(file).map_err(unreadable)).transpose())
    }

    fn run_proc_macro<'b>(&self, krate: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        self.0.run_proc_macro(krate, def, call, alloc)
    }

    fn file_left_reads(&self, krate: usize, call: blaze_scope::left_reads::CallAt<'_>, reads: &blaze_expand::Reads<'_>, span: core::ops::Range<usize>, alloc: Alloc<'_>) -> u32 {
        blaze_scope::left_reads::file_left_reads(self.0, krate, call, reads, span, alloc)
    }

    fn left_package(&self, krate: usize, left: blaze_scope::include_place::LeftPackage<'_>) {
        self.0.left_package(krate, left)
    }
}

/// The file the body of `item` is written in (the outermost call's file for a body a macro
/// wrote): where its includes are placed from (here, and super::body_left's D111 filing).
pub(super) fn body_file<'h, S: NamesSource<'h>>(names: S, krate: usize, item: LibKey) -> Option<FilePaths<'h>> {
    let place = match names.place_of(item) {
        Some(place) => place,
        // A MEMBER (body.rs `body_at` places a method's body at its member row) is no item
        // row of the items record (source/item_grain.rs `row_now` reads item rows; members
        // are `member_now`'s): its text is in its container's, written there or by a call in
        // it, so its file is the container's (the Reference, `include!` and `file!`: the file
        // of the outermost call site). tempo rt-loop a2210: `include_str!` refused (`HostError
        // Include`) in four members of crates/node/tests/it/utils.rs and zones' (`new`,
        // `reference_genesis`, `launch`, the associated const `TESTNET_REFERENCE_GENESIS`)
        // and in none of the same crate's free fns.
        None => names.place_of(names.member_owner(item)?.0)?,
    };
    if place.krate != krate {
        return None;
    }
    let At::Expansion(mut call) = place.at else {
        let At::File(file) = place.at else { return None };
        return names.file_paths(krate, file);
    };

    // The text each call is written in, outward to the outermost call's file (the Reference,
    // `include!`: relative to the file of the outermost call site): the call part of the
    // expansion's origins (`NamesSource::call_text`), which solved names and records both
    // serve. An expansion's call is no item row, so its `place_of` answers nothing over records.
    for _ in 0..super::expansion_origin::MOST_LEVELS {
        match names.call_text(krate, call)? {
            At::File(file) => return names.file_paths(krate, file),
            At::Expansion(parent) => call = parent,
        }
    }
    None
}

/// Where the text a body is lowered from stands in its file, as the library item `lib` whose
/// file and first byte the names answer ([`Placed::of`]) and the text's place in that
/// item's text: what `line!()`/`column!()` in eager input of the body's calls count from. Made
/// by the lowering, which knows the body (front2 body.rs `body_at`): a free fn's or a member's
/// own item at 0; a local item's (a fn, const or static declared in a body: tracing's
/// callsite `static`) is its outermost enclosing item's, at the local's place in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BodyAt {
    pub lib: LibKey,
    /// Where the body's text starts in `lib`'s text (`fixed`: where every call in it is).
    pub off: u32,
    /// The body's text is a copy of an expansion's (a local a macro call in the enclosing body
    /// wrote): every call in it is placed at that call, written at `off` (core `line!`: "the
    /// first macro invocation leading up to the expansion"), not at its own byte in the copy.
    pub fixed: bool,
}

/// [`BodyAt`] with its item's file text and first byte read: what [`BodyHost::place_of_call`]
/// counts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed<'h> {
    /// The whole text of the file.
    pub text: &'h str,
    /// The byte of `text` the body's text starts at (`fixed`: every call's).
    pub start: u32,
    pub fixed: bool,
}

impl<'h> Placed<'h> {
    /// `at.lib`'s place in its file (program/lazy seam_rules.rs `body_place`, the rule a
    /// `#[track_caller]` `Location` is placed by), the body's place in it added: the whole text
    /// of the file and `at.lib`'s first byte there (rule 71: turned into a line only here, when
    /// a call asks), each call at its item-relative start after it (blaze-lower by_name.rs
    /// `BodyCx`). An item or member a macro call wrote (at module level, a derive, an attribute)
    /// is at the OUTERMOST call's site, and so is every call in it, whatever `at` says (core
    /// `line!`). `None`: no place in crate `krate` (an item of another crate, a call whose
    /// origins are not kept): `line!()` refused by name.
    fn of<S: NamesSource<'h>>(names: S, krate: usize, at: BodyAt, alloc: Alloc<'_>) -> Option<Placed<'h>> {
        let (in_crate, text, start, by_call) = crate::front2::program::lazy::body_place(names, at.lib, alloc).ok()?;
        if in_crate != krate {
            return None;
        }
        if by_call {
            return Some(Placed { text, start, fixed: true });
        }
        Some(Placed { text, start: start.checked_add(at.off)?, fixed: at.fixed })
    }
}

/// The expansion host of one body: see the module's doc.
pub struct BodyHost<'h, F = fn() -> Option<FilePaths<'h>>, G = fn() -> Option<Placed<'h>>> {
    sources: Option<&'h dyn BodySources>,
    krate: usize,
    /// Only includes and file-place queries depend on the items record's file row.
    file: LazyCell<Option<FilePaths<'h>>, F>,
    /// [`Placed`], read at the first `line!()`/`column!()` a call asks.
    placed: LazyCell<Option<Placed<'h>>, G>,
    /// `file!()`'s answer, made on the first ask; `None` inside: refused.
    shown_file: core::cell::OnceCell<Option<AString<'h>>>,
    module_path: Option<&'h str>,
    atoms: &'h [(&'h str, Option<&'h str>)],
    alloc: Alloc<'h>,
}

impl<'h> BodyHost<'h> {
    /// The host of the body of `item` (a fn, or a member's container), written in `module` of
    /// the crate at `krate`; `sources` is the run's ([`super::heads::Store::sources`]): `None`
    /// refuses every env read, include and proc macro by name.
    pub fn of<S: NamesSource<'h>>(names: S, sources: Option<&'h dyn BodySources>, krate: usize, item: LibKey, module: LibKey, alloc: Alloc<'h>) -> impl ExpandCx {
        Self::of_body(names, sources, krate, item, Some(BodyAt { lib: item, off: 0, fixed: false }), module, alloc)
    }

    /// [`Self::of`] for a body the lowering placed ([`BodyAt`]: a local item's is its
    /// enclosing item's): `file!()` and the includes are `at.lib`'s file, `line!()`/`column!()`
    /// count from its place. `None`: a text with no place in a file (an anonymous constant's
    /// rendering): `file!()` is `item`'s, `line!()` refused by name.
    pub(super) fn of_body<S: NamesSource<'h>>(names: S, sources: Option<&'h dyn BodySources>, krate: usize, item: LibKey, at: Option<BodyAt>, module: LibKey, alloc: Alloc<'h>) -> impl ExpandCx {
        let file_of = at.map_or(item, |at| at.lib);
        Self::from_file_lookup(names, sources, krate, module, alloc, move || body_file(names, krate, file_of), move || at.and_then(|at| Placed::of(names, krate, at, alloc)))
    }

    /// The host of a body whose item was written by a macro. The body stage supplies the
    /// physical file of the invocation because records do not retain the expansion-origin
    /// column needed to recover it from the item key alone.
    pub fn of_with_file<S: NamesSource<'h>>(names: S, sources: Option<&'h dyn BodySources>, krate: usize, item: LibKey, module: LibKey, file: FilePaths<'h>, alloc: Alloc<'h>) -> impl ExpandCx {
        Self::from_file_lookup(
            names,
            sources,
            krate,
            module,
            alloc,
            move || body_file(names, krate, item).or_else(|| names.place_of(item).and_then(|place| matches!(place.at, At::Expansion(_)).then_some(file))),
            move || Placed::of(names, krate, BodyAt { lib: item, off: 0, fixed: false }, alloc),
        )
    }

    fn from_file_lookup<S: NamesSource<'h>, F: FnOnce() -> Option<FilePaths<'h>>, G: FnOnce() -> Option<Placed<'h>>>(
        names: S, sources: Option<&'h dyn BodySources>, krate: usize, module: LibKey, alloc: Alloc<'h>, file: F, placed: G,
    ) -> BodyHost<'h, F, G> {
        BodyHost {
            sources,
            krate,
            file: LazyCell::new(file),
            placed: LazyCell::new(placed),
            shown_file: core::cell::OnceCell::new(),
            module_path: names.module_path(module),
            atoms: names.cfg_atoms(krate).unwrap_or(&[]),
            alloc,
        }
    }

    #[cfg(test)]
    pub(super) fn planted_solved_file<S: NamesSource<'h>>(names: S, sources: &'h dyn BodySources, krate: usize, item: LibKey, module: LibKey, alloc: Alloc<'h>) -> impl ExpandCx {
        Self::from_file_lookup(names, Some(sources), krate, module, alloc, move || body_file(names.solved()?, krate, item), move || Placed::of(names, krate, BodyAt { lib: item, off: 0, fixed: false }, alloc))
    }
}

impl<'h, F: FnOnce() -> Option<FilePaths<'h>>, G: FnOnce() -> Option<Placed<'h>>> ExpandCx for BodyHost<'h, F, G> {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        self.sources.ok_or(refused(HostAsk::Env))?.env(self.krate, name)
    }

    /// The crate's cfg atoms: a predicate is true exactly when the crate is compiled with it.
    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        blaze_lower::cfg_outcome(predicate, &mut CrateCfg(self.atoms), self.alloc)
    }

    fn include(&self, written: &str, _how: IncludeAs) -> Result<Included<'_>, HostError> {
        let sources = self.sources.ok_or(refused(HostAsk::Include))?;
        if let Some(file) = sources.script_file(self.krate, written) {
            return Ok(file?.map_or(Included::Missing, Included::Bytes));
        }
        let file = (*self.file).ok_or(refused(HostAsk::Include))?;
        let package = sources.folder(self.krate).ok_or(refused(HostAsk::Include))?;
        let from = file.package.in_package(package, self.alloc).ok_or(refused(HostAsk::Include))?;
        // The one place an include's path is made, the item host's too (D96 (4): an absolute
        // path under the root the crate sees is its package's).
        let Some(path) = blaze_scope::included::included_file(package, sources.seen(self.krate), &from, written, self.alloc) else {
            return Ok(Included::Outside);
        };
        // Every kind is a read of the file's bytes, listed with the calling file (the body's
        // WHOLE read of it, rule 21): `include!` in a body is parsed as one expression (the
        // Reference, "Macros", core `include!`), so its block's meaning is the file's whole
        // text, as blaze-scope's item host reads a body's `include!` (resolve.rs
        // `Host::include`). Read as a module text it was in no read list, and an edit of the
        // file left the body's check standing.
        let bytes = sources.read_data(self.krate, &from, &path);
        Ok(bytes.map_or(Included::Missing, Included::Bytes))
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        match what {
            // D96 (4): the item host's answer, byte-equal (one read, one answer in both layers).
            PlaceOf::File => {
                let file = (*self.file).ok_or(refused(HostAsk::Place))?;
                let shown = self.shown_file.get_or_init(|| self.sources.and_then(|s| s.shown_file(self.krate, file.shown, self.alloc)));
                shown.as_ref().map(|text| Place::Text(text.as_str())).ok_or(refused(HostAsk::Place))
            }
            PlaceOf::ModulePath => self.module_path.map(Place::Text).ok_or(refused(HostAsk::Place)),
            // Not told which call asks: no item-relative position to answer (module doc); the
            // caller that knows asks `place_of_call`.
            PlaceOf::Line | PlaceOf::Column => Err(refused(HostAsk::Place)),
        }
    }

    /// `line!()`/`column!()` in eager input of the body call written at byte `call` of the
    /// body's text (tracing's `concat!(file!(), ":", line!())`, written by `event!` in a hand-
    /// written fn): counted in the file's text at the item's first byte plus `call`
    /// ([`Placed::of`]; `PlaceOf::counted`, the one count), the outermost written call as core
    /// `line!` says; in an item a macro call wrote, at module level or as a local
    /// ([`Placed::fixed`]), that call's place, whatever `call` is in the copy. Refused for a
    /// body with no place ([`Placed::of`] `None`). The answer is a read of the expansion
    /// ([`ExpandCx::place`]'s, cx.rs `Recorder::place`): the body's read list holds the line it
    /// read, as the item host's does.
    fn place_of_call(&self, what: PlaceOf, call: u32) -> Result<Place<'_>, HostError> {
        match what {
            PlaceOf::Line | PlaceOf::Column => {
                let Placed { text, start, fixed } = (*self.placed).ok_or(refused(HostAsk::Place))?;
                let unreadable = HostError { ask: HostAsk::Place, why: HostWhy::Unreadable };
                let at = if fixed { start } else { start.checked_add(call).ok_or(unreadable)? };
                what.counted(text, at as usize).map(Place::Number).ok_or(unreadable)
            }
            PlaceOf::File | PlaceOf::ModulePath => self.place(what),
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

    use super::{BodyHost, BodySources, FilePaths, LazyCell, Placed};

    /// One crate (index 0) of package `/package/3/`: `CARGO_PKG_NAME` is `tempo-node`,
    /// `CARGO_PKG_AUTHORS` is cargo's but not known, and one file besides the body's.
    struct One;

    impl BodySources for One {
        fn read(&self, krate: usize, path: &str) -> Option<&str> {
            (krate == 0 && path == "/package/3/src/abi.json").then_some("[]")
        }
        fn read_data(&self, krate: usize, from: &str, path: &str) -> Option<&[u8]> {
            (krate == 0 && from == "/package/3/src/lib.rs" && path == "/package/3/src/abi.json").then_some(b"[]")
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
        fn shown_file<'b>(&self, krate: usize, path: &str, alloc: Alloc<'b>) -> Option<blaze_own::AString<'b>> {
            // As `CrateEnv::shown_file` for the dependency tempo-node 1.0.0.
            let mut out = blaze_own::AString::new_in(alloc);
            core::fmt::Write::write_fmt(&mut out, format_args!("tempo-node-1.0.0/{path}")).ok()?;
            (krate == 0).then_some(out)
        }
        fn script_file(&self, _: usize, _: &str) -> Option<Result<Option<&[u8]>, HostError>> {
            None
        }
        fn run_proc_macro<'b>(&self, _: usize, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'b>) -> Result<Produced<'b>, HostError> {
            Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed })
        }
        // Its macros never run: nothing to file.
        fn file_left_reads(&self, _: usize, _: blaze_scope::left_reads::CallAt<'_>, _: &blaze_expand::Reads<'_>, _: core::ops::Range<usize>, _: Alloc<'_>) -> u32 {
            0
        }
        // Its one data file is in its package: nothing to tell.
        fn left_package(&self, _: usize, _: blaze_scope::include_place::LeftPackage<'_>) {}
    }

    /// A file whose second item `fn b` starts at byte 10 (line 2, column 1) and writes
    /// `tracing::info!` at its byte 13 (line 3, column 5).
    const TEXT: &str = "fn a() {}\nfn b() {\n    tracing::info!(\"x\");\n}\n";

    fn host(sources: Option<&'static dyn BodySources>) -> BodyHost<'static> {
        host_with(sources, || Some(Placed { text: TEXT, start: 10, fixed: false }))
    }

    /// [`host`] whose body is placed by `placed` ([`super::Placed::of`]'s answer).
    fn host_with(sources: Option<&'static dyn BodySources>, placed: fn() -> Option<Placed<'static>>) -> BodyHost<'static> {
        BodyHost {
            sources,
            krate: 0,
            file: LazyCell::new(|| Some(FilePaths { package: blaze_scope::package_path::PackagePath::new("src/lib.rs").unwrap(), shown: "src/lib.rs" })),
            placed: LazyCell::new(placed),
            shown_file: core::cell::OnceCell::new(),
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
        // D96 (4): a dependency's file by name and version, as the item host answers it.
        assert_eq!(h.place(PlaceOf::File), Ok(Place::Text("tempo-node-1.0.0/src/lib.rs")));
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

    /// tempo t1855d: tracing's `event!` writes `concat!(file!(), ":", line!())` in a body;
    /// `line!()` in that eager input was refused (`BodyHost::place`, no call told). Told the
    /// written call's item-relative start (blaze-lower by_name.rs `BodyCx`), the host counts in
    /// the file at the item's first byte plus it: `tracing::info!` at the body's byte 13 is
    /// line 3, column 5 (core `line!`: "the first macro invocation leading up to the
    /// expansion"; `column!`: 1-based, in characters). Twin: the body's byte 0 is the item's
    /// own line 2, column 1. Control: `place` alone still refuses `line!()`, `file!()` is the
    /// same answer either way, and an item a macro wrote (no text placed) is refused by name.
    /// Prints nothing; passes.
    #[test]
    fn a_body_call_s_line_is_its_written_call_s_in_the_file() {
        static SOURCES: One = One;
        let h = host(Some(&SOURCES));
        assert_eq!((h.place_of_call(PlaceOf::Line, 13), h.place_of_call(PlaceOf::Column, 13)), (Ok(Place::Number(3)), Ok(Place::Number(5))));
        assert_eq!((h.place_of_call(PlaceOf::Line, 0), h.place_of_call(PlaceOf::Column, 0)), (Ok(Place::Number(2)), Ok(Place::Number(1))), "twin: the item's own first byte");
        let refused = HostError { ask: HostAsk::Place, why: HostWhy::Refused };
        assert_eq!(h.place(PlaceOf::Line), Err(refused), "control: no call told");
        assert_eq!(h.place_of_call(PlaceOf::File, 13), h.place(PlaceOf::File), "control: `file!()` is the body's file either way");
        let written_by_a_macro = host_with(Some(&SOURCES), || None);
        assert_eq!(written_by_a_macro.place_of_call(PlaceOf::Line, 13), Err(refused), "control: an item a macro wrote is not placed here");
        let past = h.place_of_call(PlaceOf::Line, 1000);
        assert_eq!(past, Err(HostError { ask: HostAsk::Place, why: HostWhy::Unreadable }), "a call past the file is unreadable, never a line");
    }

    /// tempo rt-loop a2000 (394 items not checked): tracing's `event!` writes its callsite
    /// `static` in the user's fn, and the static's initializer holds `concat!(file!(), ":",
    /// line!())`. Since local statics are checked as bodies of their own (every_body.rs), that
    /// initializer was lowered with a host placed at the LOCAL key, which the names do not place:
    /// `line!()` was refused. Placed as a copy at the outermost call (`fixed`: the `info!` at
    /// byte 13 of `fn b`, file byte 23), every call in the copy is that call's line 3, column 5
    /// (core `line!`: "the first macro invocation leading up to the expansion"), whatever its
    /// byte in the copy. Twin: the same place not fixed counts the call's own byte (byte 0 is
    /// line 3, column 5; byte 5 is column 10). Control: `place` alone still refuses `line!()`.
    /// Prints nothing; passes.
    #[test]
    fn a_call_in_a_local_a_macro_wrote_is_placed_at_the_outermost_call() {
        static SOURCES: One = One;
        let copy = host_with(Some(&SOURCES), || Some(Placed { text: TEXT, start: 23, fixed: true }));
        assert_eq!((copy.place_of_call(PlaceOf::Line, 0), copy.place_of_call(PlaceOf::Column, 0)), (Ok(Place::Number(3)), Ok(Place::Number(5))));
        assert_eq!((copy.place_of_call(PlaceOf::Line, 400), copy.place_of_call(PlaceOf::Column, 400)), (Ok(Place::Number(3)), Ok(Place::Number(5))), "any byte of the copy: the call's place");
        let written = host_with(Some(&SOURCES), || Some(Placed { text: TEXT, start: 23, fixed: false }));
        assert_eq!((written.place_of_call(PlaceOf::Line, 0), written.place_of_call(PlaceOf::Column, 5)), (Ok(Place::Number(3)), Ok(Place::Number(10))), "twin: a written local counts its own byte");
        assert_eq!(copy.place(PlaceOf::Line), Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused }), "control: no call told");
    }
}
