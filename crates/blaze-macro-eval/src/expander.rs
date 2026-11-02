//! [`load`]: a proc-macro crate's macros, each an expander that runs the macro's own function
//! ([`run`](crate::run)) and keeps what it gave in the store.
//!
//! A call is keyed by the macro's code (its crate's whole closure and this server's version,
//! [`code`](crate::code)), what it is given (name, kind, input and attribute tokens, spans as
//! shapes, the calling crate's variables and directory, the toolchain: `proc_macro_api::kept::given`),
//! and what it read (files and variables, traced by [`os`](crate::os) and looked up again on
//! a hit). Not kept: one that asked the database about a span, read what can't be traced,
//! emitted a diagnostic, failed, or ran with state from the calls before it.
//!
//! State: a macro whose run changed a static outside the standard library has state between
//! calls, as it has under rustc, where each calling crate's compilation loads it once. Each
//! calling crate's calls of it are a session, in the order they expand, started again with each
//! revision of the database. Statics that hold no address are given to the next call as they
//! were left; when one holds an address, which no fresh run can be given, the session's earlier
//! calls are run again on the same evaluator before the next one.
//!
//! Whether a crate's macros keep state is learned, as the wasm host learns it, and kept in the
//! store by the crate's code: unseen, its calls expand in a session (`requires_session`), in
//! declaration order, and no kept expansion is used, since a hit skips a run that may have left
//! state; once a build ran some and none left state ([`flush`]), it is stateless and its calls
//! are found in the store; once one left state, it is stateful for good.

use std::{
    fmt,
    sync::{Arc, Weak},
};

use base_db::{Crate, Db, Nonce, ProcMacroLoadingError};
use blaze_hash::SharedMap;
use blaze_ids::ContentId;
use blaze_usercode::{
    RunKey,
    index::{self, FindError, Found},
    trace::{Seen, assemble_seen},
};
use hir_def::{FunctionId, StaticId, nameres::crate_def_map, unstable_features::UnstableFeatures};
use hir_expand::proc_macro::{
    ExpansionReads, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
    ProcMacroKind, ProcMacroLoadResult, report_reads,
};
use intern::Symbol;
use parking_lot::Mutex;
use proc_macro_api::kept::verdict::{self as verdicts, STATEFUL, STATELESS, Verdict};
use proc_macro_api::kept::{Call, Keeping, Mode, Put, checked, given, keeping, put, read, written};

use crate::{
    code,
    keep::Calling,
    protocol,
    run::{self, Bridge, Expansion, Ran},
    stream::Stream,
};

/// What the macros of one proc-macro crate share.
struct Library {
    krate: Crate,
    /// The crate's name, as an error about one of its macros says it.
    name: Box<str>,
    toolchain: ContentId,
    bridge: Bridge,
    /// The code as it was at [`load`], which is asked again each time the crate compiles.
    loaded: ContentId,
    code: code::Memo,
    /// What the store said of the crate's state when it was loaded, and what its expansions
    /// that ran (not found) did since.
    seen: verdicts::Seen,
    /// Each calling crate's session, each held while a call that has state runs: its calls
    /// run in order.
    sessions: SharedMap<Crate, Arc<Mutex<Session>>>,
}

/// A call that panicked holding a session or the sessions leaves them as they were at the panic,
/// and the next call takes them as they are.
impl std::panic::RefUnwindSafe for Library {}

/// The run a verdict of the crate whose code is `code` is kept under, in the store expansions
/// are kept in, with nothing read. Each verdict has its own key, so neither is ever put under a
/// key that holds the other, and a crate seen stateful by one build and stateless by another is
/// stateful.
fn verdict_run(code: &ContentId, verdict: &[u8]) -> RunKey {
    let given = blaze_ids::Hasher::new("whether a proc-macro crate on blaze's evaluator keeps state 2")
        .part(verdict)
        .finish();
    RunKey { code: *code, given }
}

/// Whether the store says `verdict` of the crate whose code is `code`.
fn said(keeping: &Keeping, code: &ContentId, verdict: &[u8]) -> Result<bool, String> {
    let guard = blaze_cas::enter();
    let env = base_db::Env::default();
    match index::find(&keeping.expansions.store, &guard, &verdict_run(code, verdict), &mut Calling::new(&env)) {
        Ok(Found::Hit { value, .. }) if value == verdict => Ok(true),
        // alloc: error path
        Ok(Found::Hit { value, .. }) => Err(format!("the store keeps {value:?} as its verdict, not {verdict:?}")),
        Ok(Found::Miss { .. }) => Ok(false),
        // alloc: error path
        Err(FindError::Corrupt(corrupt)) => Err(corrupt.to_string()),
        // alloc: error path
        Err(FindError::NotAShape { key, why }) => Err(format!("the store's {key:?} is not a shape: {why}")),
        // alloc: error path
        Err(FindError::Inputs(unreadable)) => Err(unreadable.to_string()),
    }
}

/// What the store says of the crate whose code is `code`. With nothing installed to keep
/// expansions in (a bench, a test that keeps nothing), nothing is known and nothing is found.
fn verdict(code: &ContentId) -> Result<Verdict, String> {
    let Some(keeping) = keeping() else { return Ok(Verdict::Unseen) };
    if said(&keeping, code, STATEFUL)? {
        return Ok(Verdict::Stateful);
    }
    Ok(if said(&keeping, code, STATELESS)? { Verdict::Stateless } else { Verdict::Unseen })
}

/// Leaves `verdict` of `library` for the store's owner to publish with this thread's puts.
fn tell(library: &Library, verdict: &'static [u8]) {
    if keeping().is_none() {
        return;
    }
    put(Put {
        run: verdict_run(&library.loaded, verdict),
        traced: blaze_core::Traced::default(),
        value: verdict.to_vec(), // alloc: once per crate per build, a verdict
        // alloc: as above
        what: format!("the verdict of the proc macros of `{}`", library.name),
    });
}

/// Every library loaded, for [`flush`].
static LIBRARIES: Mutex<Vec<Weak<Library>>> = Mutex::new(Vec::new()); // alloc: none: an empty `Vec` in a const

/// Tells the store of each crate whose macros ran in this build and left no state that it is
/// stateless: called once a build has expanded everything, on the thread of the store's
/// owner, which then publishes this thread's puts (`proc_macro_api::kept::take_puts`). Writes nothing
/// itself and blocks on nothing.
pub fn flush() {
    if keeping().is_none() {
        return;
    }
    let mut libraries = LIBRARIES.lock();
    libraries.retain(|it| it.strong_count() != 0);
    for library in libraries.iter().filter_map(Weak::upgrade) {
        // Another build that saw one leave state may have told the store since the load.
        if library.seen.flush() && verdict(&library.loaded) == Ok(Verdict::Unseen) {
            tell(&library, STATELESS);
        }
    }
}

impl Library {
    /// Notes what a run did: one that left state is told the store with the run's puts. One
    /// that left state where the store said the crate keeps none is an error: this build's
    /// calls of it ran, or were reused, as if each started from nothing.
    fn saw(&self, ran: &Ran) -> Result<(), ProcMacroExpansionError> {
        let Some(contradicted) = self.seen.saw(!ran.statics.is_empty()) else { return Ok(()) };
        tell(self, STATEFUL);
        if contradicted {
            // alloc: error path
            return Err(ProcMacroExpansionError::System(format!(
                "the proc macros of `{}` keep state between expansions, which the store said they \
                 did not: this build's expansions of them may not be what rustc gives; they expand \
                 in a session from the next load",
                self.name
            )));
        }
        Ok(())
    }

    /// The session of `calling` in the database's revision now, started again when it was of
    /// another.
    fn session(&self, db: &Db, calling: Crate) -> Arc<Mutex<Session>> {
        let now = db.nonce_and_revision();
        // clone: the session's handle, held for one call
        let session = Arc::clone(self.sessions.pin().get_or_insert_with(calling, Arc::default));
        {
            let mut held = session.lock();
            if held.at != Some(now) {
                *held = Session { at: Some(now), ..Session::default() };
            }
        }
        session
    }
}

impl fmt::Debug for Library {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Library").field("name", &self.name).field("loaded", &self.loaded).finish_non_exhaustive()
    }
}

/// One calling crate's calls of a library's macros in one revision.
#[derive(Default)]
struct Session {
    at: Option<(Nonce, blaze_query::db::View)>,
    /// The statics the calls so far left that hold no address.
    statics: Vec<(StaticId, Vec<u8>)>,
    /// Whether a static they left holds an address: then [`Session::calls`] are run again.
    holds: bool,
    /// The calls so far, while [`Session::holds`] can still become true or is.
    calls: Vec<Kept>,
}

/// A call kept for running it again.
struct Kept {
    function: FunctionId,
    attr: bool,
    inputs: Vec<tt::TopSubtree>,
    sites: [span::Span; 3],
    edition: span::Edition,
    forced_keywords: bool,
}

#[derive(Debug)]
struct Evaluated {
    library: Arc<Library>,
    name: Symbol,
    kind: ProcMacroKind,
    function: FunctionId,
}

fn refused(why: String) -> ProcMacroLoadingError {
    ProcMacroLoadingError::Refused(why.into_boxed_str())
}

/// The macros of `krate`, a proc-macro crate, each run on blaze's compiler in this process
/// for a crate built with `toolchain`. An error says why none can run: the crate is no
/// proc-macro crate, or its `proc_macro` is not the bridge blaze's server speaks.
/// The name of `krate` as its errors and spans say it.
fn crate_name(db: &Db, krate: Crate) -> Box<str> {
    match &krate.extra_data(db).display_name {
        Some(name) => name.to_string().into(), // alloc: once per name asked for
        None => "<unnamed>".into(),
    }
}

pub fn load(db: &Db, krate: Crate, toolchain: ContentId) -> ProcMacroLoadResult {
    // alloc: once a proc-macro crate, the name its errors say
    let name = crate_name(db, krate);
    if !crate_def_map(db, krate).is_proc_macro() {
        return Err(refused(format!("`{name}` is not a proc-macro crate"))); // alloc: error path
    }
    let Some(proc_macro) = protocol::proc_macro_of(db, krate) else {
        return Err(refused(format!("the proc-macro crate `{name}` does not depend on `proc_macro`"))); // alloc: error path
    };
    protocol::check(db, proc_macro).map_err(|why| refused(format!("the proc-macro crate `{name}`: {why}")))?; // alloc: error path
    let bridge = Bridge::find(db, proc_macro).map_err(|why| refused(format!("the proc-macro crate `{name}`: {why}")))?; // alloc: error path
    let code = code::Memo::default();
    let loaded = code.get(db, krate);
    let verdict = verdict(&loaded)
        // alloc: error path
        .map_err(|why| refused(format!("the proc-macro crate `{name}`: its verdict in the store: {why}")))?;
    let library = Arc::new(Library { // alloc: once a proc-macro crate loaded
        krate,
        name,
        toolchain,
        bridge,
        loaded,
        code,
        seen: verdicts::Seen::new(verdict),
        sessions: SharedMap::default(),
    });
    LIBRARIES.lock().push(Arc::downgrade(&library));
    let functions = run::functions(db, krate);
    if functions.is_empty() {
        return Err(refused(format!("the proc-macro crate `{}` has no proc macros", library.name))); // alloc: error path
    }
    Ok(functions
        .into_iter()
        .map(|(name, kind, function)| ProcMacro {
            name: name.clone(), // clone: a symbol, once a macro
            kind,
            // alloc: once a macro loaded
            expander: Arc::new(Evaluated { library: Arc::clone(&library), name, kind, function }),
            disabled: false,
        })
        .collect()) // alloc: once a proc-macro crate, its macros
}

/// Whether `calling` enables `forced_keywords`, read without its def map: its proc macros
/// expand while that is built.
fn forced_keywords(db: &Db, calling: Crate) -> bool {
    UnstableFeatures::of_root(db, calling).forced_keywords
}

fn system(why: impl fmt::Display) -> ProcMacroExpansionError {
    // alloc: error path
    ProcMacroExpansionError::System(why.to_string())
}

impl Evaluated {
    /// This macro, as an error about it begins.
    fn what(&self) -> String {
        // alloc: an error, or what a put said owns
        format!("the proc macro `{}` of `{}`", self.name.as_str(), self.library.name)
    }

    /// The inputs of `call`: the attribute's arguments first for an attribute.
    fn inputs(&self, call: &ProcMacroCall<'_>) -> Vec<tt::TopSubtree> {
        // alloc: once per call, the trees kept for the session or made into streams
        let mut inputs = Vec::with_capacity(2);
        if self.kind == ProcMacroKind::Attr {
            match call.attrs {
                Some(attrs) => inputs.push(attrs.copy()),
                None => inputs.push(tt::TopSubtree::empty(tt::DelimSpan { open: call.call_site, close: call.call_site })),
            }
        }
        inputs.push(call.subtree.copy());
        inputs
    }

    /// Runs the call, after the session's calls before it when they must be run again, and
    /// keeps the state it leaves in the session.
    fn run(&self, db: &Db, calling: Crate, call: &ProcMacroCall<'_>) -> Result<Ran, ProcMacroExpansionError> {
        let library = &*self.library;
        let session = library.session(db, calling);
        let mut held = session.lock();
        let this = Kept {
            function: self.function,
            attr: self.kind == ProcMacroKind::Attr,
            inputs: self.inputs(call),
            sites: [call.def_site, call.call_site, call.mixed_site],
            edition: call.call_site.anchor.file_id.edition(),
            forced_keywords: forced_keywords(db, calling),
        };
        let stateless = held.statics.is_empty() && !held.holds;
        let ran = if stateless {
            // Its calls run at once: the state of each is the start, which none of them change.
            drop(held);
            let ran = self.run_on(db, calling, call, &[], &[], &this);
            held = session.lock();
            ran?
        } else {
            let replayed: &[Kept] = if held.holds { &held.calls } else { &[] };
            let preset: &[(StaticId, Vec<u8>)] = if held.holds { &[] } else { &held.statics };
            self.run_on(db, calling, call, preset, replayed, &this)?
        };
        library.saw(&ran)?;
        let session = &mut *held;
        if !ran.statics.is_empty() {
            session.holds |= ran.statics.iter().any(|it| it.2);
            if !session.holds {
                for (st, bytes, _) in &ran.statics {
                    match session.statics.iter_mut().find(|it| it.0 == *st) {
                        // clone: a changed static, kept for the session's next call
                        Some(it) => it.1.clone_from(bytes),
                        None => session.statics.push((*st, bytes.clone())), // clone: as above
                    }
                }
            }
        }
        if !session.statics.is_empty() || session.holds {
            session.calls.push(this);
        }
        Ok(ran)
    }

    /// Runs `this` after `replayed`, from the statics `preset`.
    #[allow(clippy::too_many_arguments)]
    fn run_on(
        &self,
        db: &Db,
        calling: Crate,
        call: &ProcMacroCall<'_>,
        preset: &[(StaticId, Vec<u8>)],
        replayed: &[Kept],
        this: &Kept,
    ) -> Result<Ran, ProcMacroExpansionError> {
        // alloc: once per call, its streams and those of the calls run again
        let streams: Vec<Vec<Stream>> = replayed
            .iter()
            .chain(std::iter::once(this))
            .map(|it| it.inputs.iter().map(Stream::of_call).collect()) // alloc: once per run, its input streams
            .collect(); // alloc: once per run, as above
        let expansions: Vec<Expansion<'_>> = replayed
            .iter()
            .chain(std::iter::once(this))
            .zip(&streams)
            .map(|(it, inputs)| Expansion {
                function: it.function,
                attr: it.attr,
                inputs,
                sites: it.sites,
                edition: it.edition,
                forced_keywords: it.forced_keywords,
            })
            .collect(); // alloc: once per run, the expansions it runs
        let library = &*self.library;
        // alloc: once per call, the names its span carries
        let path = format!("{}::{}", library.name, self.name.as_str());
        let calling = crate_name(db, calling);
        let span = run::expand_span(&path, &calling);
        // alloc: once per call, what its progress lines say
        let what = format!("the proc macro `{path}` for `{calling}`");
        let mut spent = run::Spent::default();
        let ran = run::run(db, &what, &library.bridge, call.env, preset, &expansions, &mut spent);
        run::ran_on(span, &spent, &ran);
        ran.map_err(|stop| system(format_args!("{} {stop}", self.what())))
    }

    /// Whether the session of `calling` starts each call from nothing.
    fn stateless(&self, db: &Db, calling: Crate) -> bool {
        let session = self.library.session(db, calling);
        let held = session.lock();
        held.statics.is_empty() && !held.holds
    }

    /// The output of `ran`, or the error it ends in.
    fn output(&self, ran: &Ran, call_site: span::Span) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let errors: Vec<&str> = ran.emitted.iter().filter(|it| it.error).map(|it| it.text.as_str()).collect(); // alloc: once per call, the errors it emitted (none for most)
        for warning in ran.emitted.iter().filter(|it| !it.error) {
            eprintln!("{}: {}", self.what(), warning.text);
        }
        match &ran.output {
            Err(panic) => Err(ProcMacroExpansionError::Panic(panic.clone())), // clone: error path
            Ok(_) if !errors.is_empty() => Err(ProcMacroExpansionError::Panic(errors.join("\n"))),
            // clone: a stream shares its trees, so this copies a pointer
            Ok(stream) => Ok(stream.clone().into_top(call_site)),
        }
    }

    /// The lines of what `ran` read, with the paths it tracked made absolute against `dir`,
    /// and the files outside the source roots its macro's code was made from (`included`).
    fn lines<'a>(
        ran: &'a Ran,
        dir: &str,
        included: &[std::path::PathBuf],
        tracked: &'a mut Vec<String>,
    ) -> impl Iterator<Item = (Seen, &'a str)> {
        tracked.clear();
        tracked.extend(ran.tracked.iter().map(|path| {
            // alloc: once per tracked path, as an expansion is kept
            std::path::Path::new(dir).join(path).to_string_lossy().into_owned()
        }));
        // alloc: once per included file, as an expansion is kept
        tracked.extend(included.iter().map(|path| path.to_string_lossy().into_owned()));
        ran.seen.iter().map(|(seen, what)| (*seen, what.as_str())).chain(tracked.iter().map(|it| (Seen::Read, it.as_str())))
    }

    fn expand_keyed(&self, db: &Db, calling: Crate, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let Some(keeping) = keeping() else {
            let ran = self.run(db, calling, &call)?;
            report_reads(ExpansionReads::Unknown("expansions are not kept here"));
            return self.output(&ran, call.call_site);
        };
        let library = &*self.library;
        let stateless = self.stateless(db, calling);
        let keyed = Call {
            name: self.name.as_str(),
            kind: self.kind,
            subtree: call.subtree,
            attrs: call.attrs,
            env: call.env,
            def_site: call.def_site,
            call_site: call.call_site,
            mixed_site: call.mixed_site,
            current_dir: call.current_dir,
        };
        let (mut given, spans) = given(&library.toolchain, &keyed);
        // The server lexes the text a macro parses in the call site's edition; the spans'
        // shapes number their anchors, which says no edition.
        given.part("edition", &[call.call_site.anchor.file_id.edition() as u8]);
        // Whether each `k#` the macro lexes is gated, which the crate's attributes say.
        given.part("forced_keywords", &[forced_keywords(db, calling) as u8]);
        let key = RunKey { code: library.code.get(db, library.krate), given: given.finish() };
        let mut inputs = Calling::new(call.env);
        let mut found = None;
        if stateless && library.seen.verdict() == Verdict::Stateless {
            let guard = blaze_cas::enter();
            match index::find_reads(&keeping.expansions.store, &guard, &key, &mut inputs) {
                Ok(Found::Hit { value, shape, values: Some(values), .. }) => {
                    // alloc: what the query reporting it owns
                    let reads = ExpansionReads::Traced { shape: shape.into(), values };
                    match keeping.mode {
                        Mode::Keep => {
                            let output = read(value, &spans).map_err(|why| system(format_args!("{}: {why}", self.what())))?;
                            report_reads(reads);
                            return Ok(output);
                        }
                        Mode::Check => found = Some((checked(value), reads)),
                    }
                }
                Ok(Found::Hit { values: None, .. }) => {
                    return Err(system(format_args!("{}: the store's lookup found a hit without its values", self.what())));
                }
                Ok(Found::Miss { .. }) => {}
                Err(FindError::Corrupt(corrupt)) => return Err(system(format_args!("{}: {corrupt}", self.what()))),
                Err(FindError::NotAShape { key, why }) => {
                    return Err(system(format_args!("{}: the store's {key:?} is not a shape: {why}", self.what())));
                }
                Err(FindError::Inputs(unreadable)) => return Err(system(format_args!("{}: {unreadable}", self.what()))),
            }
        }
        let ran = self.run(db, calling, &call)?;
        let output = self.output(&ran, call.call_site)?;
        let bytes = written(&output, &spans);
        if let Some((kept, reads)) = found {
            if bytes.as_deref().map(checked) != Some(kept) {
                return Err(system(format_args!(
                    "{}: {}=check: it expanded otherwise than the store kept it, under a key of \
                     every input it was traced to read",
                    self.what(),
                    proc_macro_api::kept::CACHE
                )));
            }
            report_reads(reads);
            return Ok(output);
        }
        let unknown = |why: &'static str| {
            report_reads(ExpansionReads::Unknown(why));
        };
        if ran.asked {
            unknown("it asked the database");
            return Ok(output);
        }
        if !stateless || !ran.statics.is_empty() || library.seen.verdict() == Verdict::Stateful {
            unknown("it ran with state its calls before it left");
            return Ok(output);
        }
        if !ran.emitted.is_empty() {
            unknown("it emitted a diagnostic");
            return Ok(output);
        }
        if ran.unkeyable.is_some() {
            unknown("what it read can't be keyed");
            return Ok(output);
        }
        // alloc: once per kept expansion, its tracked paths made absolute
        let mut tracked = Vec::new();
        // alloc: as above, the files the macro's code included
        let mut included = Vec::new();
        code::included(db, library.krate, &mut included);
        let traced = assemble_seen(Self::lines(&ran, call.current_dir, &included, &mut tracked), &mut inputs)
            .map_err(|why| system(format_args!("{}: {why}", self.what())))?;
        if index::untraceable(&traced) {
            unknown("it read what can't be traced (the clock, randomness)");
            return Ok(output);
        }
        report_reads(ExpansionReads::Traced {
            shape: index::Shape::of(&traced).encode().into(), // alloc: once, as an expansion is kept
            values: index::VALUES.traced(&traced),
        });
        let Some(value) = bytes else {
            return Ok(output);
        };
        put(Put { run: key, traced, value, what: self.what() });
        Ok(output)
    }
}

impl ProcMacroExpander for Evaluated {
    fn requires_session(&self) -> bool {
        self.library.seen.verdict() != Verdict::Stateless
    }

    fn code(&self) -> Option<ContentId> {
        Some(self.library.loaded)
    }

    /// A call with no calling crate is its own session.
    fn expand(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let calling = self.library.krate;
        self.expand_keyed(db, calling, call)
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        self.expand_keyed(db, calling_crate, call)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: the database's expander trait, the edge
        (other as &dyn std::any::Any) // dyn: as above, told apart by type
            .downcast_ref::<Evaluated>()
            .is_some_and(|other| {
                Arc::ptr_eq(&self.library, &other.library) && self.function == other.function
            })
    }
}
