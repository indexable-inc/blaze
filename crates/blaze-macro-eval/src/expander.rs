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
//! A crate's wasm module ([`load_wasm`]) runs each expansion in a fresh instance, so it keeps
//! no state and is stateless from its load; its expansions are kept apart from the evaluator's,
//! under the module's id.
//!
//! Whether a crate's macros keep state on the evaluator is learned, and kept in the
//! store by the crate's code: unseen, its calls expand in a session (`requires_session`), in
//! declaration order, and no kept expansion is used, since a hit skips a run that may have left
//! state; once a build ran some and none left state ([`Macros::flush`]), it is stateless and its calls
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
    roots::{Role, RootMap, Roots, verify},
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
use proc_macro_api::kept::{Call, Keep, Keeping, Keeps, Mode, Put, checked, given, put, read, written};

use crate::{
    code,
    keep::Calling,
    protocol,
    run::{self, Bridge, Expansion, Ran},
    stream::Stream,
    wasm,
};

/// What a daemon's proc macros share: where their expansions are kept, and every library
/// loaded, which [`Macros::flush`] tells the store of. Made once by the daemon and lent to every
/// [`load`] and [`load_wasm`]; two in one process see neither each other's store nor each
/// other's libraries. Its libraries are held weakly, so a library holding it makes no cycle.
pub struct Macros {
    keeps: Keeps,
    libraries: Mutex<Vec<Weak<Library>>>,
    /// The daemon's wasm modules ([`load_wasm`]'s), made with it: or why Wasmtime made no
    /// engine, which every wasm macro's load then says.
    modules: Result<wasm::Modules, String>,
}

const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Macros>();
};

impl Macros {
    pub fn new(keeps: Keeps) -> Arc<Macros> {
        // alloc: once per daemon, or per test that loads macros
        Arc::new(Macros { keeps, libraries: Mutex::new(Vec::new()), modules: wasm::Modules::new() })
    }

    /// The daemon's wasm modules, which a proc macro built for wasm loads through
    /// ([`load_wasm`]): or why there are none, Wasmtime having made no engine.
    pub fn modules(&self) -> Result<&wasm::Modules, &str> {
        self.modules.as_ref().map_err(String::as_str)
    }

    /// Macros that keep and find nothing, for `why` (a bench, a test of the evaluator alone),
    /// which each expansion that runs says of its reads.
    pub fn nothing(why: &'static str) -> Arc<Macros> {
        Macros::new(Keeps::Nothing(why))
    }

    /// Where its expansions are kept.
    pub fn keeps(&self) -> &Keeps {
        &self.keeps
    }

    /// Tells the store of each crate whose macros ran in this build and left no state that it
    /// is stateless: called once a build has expanded everything, on the thread of the store's
    /// owner, which then publishes this thread's puts (`proc_macro_api::kept::take_puts`).
    /// Writes nothing itself and blocks on nothing.
    pub fn flush(&self) {
        if let Keeps::Nothing(_) = self.keeps {
            return;
        }
        let mut libraries = self.libraries.lock();
        libraries.retain(|it| it.strong_count() != 0);
        for library in libraries.iter().filter_map(Weak::upgrade) {
            // Another build that saw one leave state may have told the store since the load.
            if library.seen.flush() && verdict(&self.keeps, &library.loaded) == Ok(Verdict::Unseen) {
                tell(&library, STATELESS);
            }
        }
    }
}

impl fmt::Debug for Macros {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keeps = match &self.keeps {
            Keeps::In(keeping) => format!("in a store, {:?}", keeping.mode), // alloc: debug output
            Keeps::Nothing(why) => format!("nothing: {why}"), // alloc: debug output
        };
        f.debug_struct("Macros").field("keeps", &keeps).field("libraries", &self.libraries.lock().len()).finish()
    }
}

/// What the macros of one proc-macro crate share.
struct Library {
    /// The daemon's, which says where this crate's expansions are kept.
    macros: Arc<Macros>,
    krate: Crate,
    /// The crate's name, as an error about one of its macros says it.
    name: Box<str>,
    toolchain: ContentId,
    engine: Engine,
    /// The code as it was at [`load`], which is asked again each time the crate compiles.
    loaded: ContentId,
    code: code::Memo,
    /// What the store said of the crate's state when it was loaded, and what its expansions
    /// that ran (not found) did since.
    seen: verdicts::Seen,
    /// Each calling crate's session, each held while a call that has state runs: its calls
    /// run in order.
    sessions: SharedMap<Crate, Arc<Mutex<Session>>>,
    /// Each calling crate's roots as its expansions are kept against them ([`Library::kept_roots`]).
    kept_roots: SharedMap<Crate, Arc<KeptRoots>>,
}

/// A calling crate's roots with their other spellings and machine-wide directories, and what
/// they were made of: the database's revision (a root's links are read again in each), the
/// package and `OUT_DIR` and the passed `CARGO_HOME` and `HOME`. Made again when any differs.
struct KeptRoots {
    at: (Nonce, blaze_query::db::View),
    made_of: [Option<String>; 4],
    map: RootMap,
}

/// What a library's macros run on.
enum Engine {
    /// The MIR evaluator, through the bridge's functions in the crate's `proc_macro`.
    Evaluator(Bridge),
    /// The crate's wasm module ([`load_wasm`]). Each expansion is a fresh instance, so the
    /// library is stateless from its load.
    Wasm(wasm::Library),
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
        // alloc: error path
        Err(FindError::TooManyShapes { run }) => Err(format!("verdict run {run:?} has more than {} shapes", index::MOST_SHAPES)),
    }
}

/// What the store `keeps` says of the crate whose code is `code`. Keeping nothing (a bench, a
/// test that keeps nothing), nothing is known and nothing is found.
fn verdict(keeps: &Keeps, code: &ContentId) -> Result<Verdict, String> {
    let Keeps::In(keeping) = keeps else { return Ok(Verdict::Unseen) };
    if said(keeping, code, STATEFUL)? {
        return Ok(Verdict::Stateful);
    }
    Ok(if said(keeping, code, STATELESS)? { Verdict::Stateless } else { Verdict::Unseen })
}

/// Leaves `verdict` of `library` for the store's owner to publish with this thread's puts.
fn tell(library: &Library, verdict: &'static [u8]) {
    if let Keeps::Nothing(_) = library.macros.keeps {
        return;
    }
    put(Put {
        keep: Keep::Run { run: verdict_run(&library.loaded, verdict), traced: blaze_core::Traced::default() },
        value: verdict.to_vec(), // alloc: once per crate per build, a verdict
        // alloc: as above
        what: format!("the verdict of the proc macros of `{}`", library.name),
    });
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

    /// The roots `calling`'s expansion with `roots` is kept against, given `passed`: made once
    /// per calling crate and revision ([`KeptRoots`]), not once per expansion, as making them
    /// reads each root's links.
    fn kept_roots(&self, db: &Db, calling: Crate, roots: Roots<'_>, passed: &[(String, String)]) -> Arc<KeptRoots> {
        let at = db.nonce_and_revision();
        let var = |name: &str| passed.iter().rev().find(|it| it.0 == name).map(|it| it.1.as_str());
        let made_of = [roots.get(Role::Package), roots.get(Role::OutDir), var("CARGO_HOME"), var("HOME")];
        let same = |kept: &KeptRoots| kept.at == at && kept.made_of.iter().map(Option::as_deref).eq(made_of.iter().copied());
        let pinned = self.kept_roots.pin();
        if let Some(kept) = pinned.get(&calling).filter(|it| same(it)) {
            return Arc::clone(kept); // clone: the roots' handle, held for one expansion
        }
        let kept = Arc::new(KeptRoots {
            // alloc: once per calling crate and revision
            at,
            made_of: made_of.map(|it| it.map(str::to_owned)), // alloc: as above
            map: roots.to_map(|map| map.with_cargo_home(var)),
        });
        pinned.insert(calling, Arc::clone(&kept)); // clone: as above
        kept
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
    /// Its place in the crate's `__blaze_proc_macros` list, sorted by name, as a wasm module
    /// numbers its macros.
    index: u32,
}

fn refused(why: String) -> ProcMacroLoadingError {
    ProcMacroLoadingError::Refused(why.into_boxed_str())
}

/// The name of `krate` as its errors and spans say it.
fn crate_name(db: &Db, krate: Crate) -> Box<str> {
    match &krate.extra_data(db).display_name {
        Some(name) => name.to_string().into(), // alloc: once per name asked for
        None => "<unnamed>".into(),
    }
}

/// The macros of `krate`, a proc-macro crate, each run on blaze's MIR evaluator in this
/// process for a crate built with `toolchain`. An error says why none can run: the crate is no
/// proc-macro crate, or its `proc_macro` is not the bridge blaze's server speaks. Its expansions
/// are kept where the daemon's `macros` keeps them.
pub fn load(db: &Db, krate: Crate, toolchain: ContentId, macros: &Arc<Macros>) -> ProcMacroLoadResult {
    load_on(db, krate, toolchain, macros, |name, checked| {
        let verdict = verdict(&macros.keeps, &checked.loaded)
            // alloc: error path
            .map_err(|why| refused(format!("the proc-macro crate `{name}`: its verdict in the store: {why}")))?;
        Ok((Engine::Evaluator(checked.bridge), verdict))
    })
}

/// The macros of `krate`, each run in a fresh instance of `module`, the crate built by blaze
/// for `wasm32-wasip1` with the bridge's glue (`guest/entry.rs`), under Wasmtime. The module
/// comes from `modules`, the daemon's, which compiles it or loads its machine code from the
/// store the daemon's `macros` keeps expansions in. An error says why none can run: [`load`]'s,
/// or the module's, naming the crate; nothing runs on the evaluator instead.
pub fn load_wasm(
    db: &Db,
    krate: Crate,
    toolchain: ContentId,
    macros: &Arc<Macros>,
    modules: &wasm::Modules,
    module: &[u8],
) -> ProcMacroLoadResult {
    load_on(db, krate, toolchain, macros, |name, _| {
        // alloc: error path, and once a crate the name its put says
        let what = format!("the wasm module of the proc-macro crate `{name}`");
        let library = wasm::Library::load(modules, &macros.keeps, module, &what).map_err(|why| refused(format!("{what} {why}")))?; // alloc: error path
        Ok((Engine::Wasm(library), Verdict::Stateless))
    })
}

/// What [`load_on`] found of a crate for its engine: the bridge's functions, and its code.
struct Checked {
    bridge: Bridge,
    loaded: ContentId,
}

/// [`load`] and [`load_wasm`]: the crate checked, and its macros on the engine (and the
/// verdict of its state) `engine` makes of the crate's name and what was found of it.
fn load_on(
    db: &Db,
    krate: Crate,
    toolchain: ContentId,
    macros: &Arc<Macros>,
    engine: impl FnOnce(&str, Checked) -> Result<(Engine, Verdict), ProcMacroLoadingError>,
) -> ProcMacroLoadResult {
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
    let (engine, verdict) = engine(&name, Checked { bridge, loaded })?;
    let library = Arc::new(Library { // alloc: once a proc-macro crate loaded
        macros: Arc::clone(macros), // clone: a refcount, once a proc-macro crate loaded
        krate,
        name,
        toolchain,
        engine,
        loaded,
        code,
        seen: verdicts::Seen::new(verdict),
        sessions: SharedMap::default(),
        kept_roots: SharedMap::default(),
    });
    macros.libraries.lock().push(Arc::downgrade(&library));
    let functions = run::functions(db, krate);
    if functions.is_empty() {
        return Err(refused(format!("the proc-macro crate `{}` has no proc macros", library.name))); // alloc: error path
    }
    // A module numbers its macros by their place in `__blaze_proc_macros`, which is sorted by
    // name (`hir_ty_codegen::v2::roots::proc_macros`).
    // alloc: once a proc-macro crate, its macros' places
    let mut by_name: Vec<usize> = (0..functions.len()).collect();
    by_name.sort_by(|&a, &b| functions[a].0.as_str().cmp(functions[b].0.as_str()));
    let mut places = vec![0u32; functions.len()]; // alloc: as above
    for (place, &at) in by_name.iter().enumerate() {
        places[at] = place as u32;
    }
    Ok(functions
        .into_iter()
        .zip(places)
        .map(|((name, kind, function), index)| ProcMacro {
            name: name.clone(), // clone: a symbol, once a macro
            kind,
            // alloc: once a macro loaded
            expander: Arc::new(Evaluated { library: Arc::clone(&library), name, kind, function, index }),
            disabled: false,
        })
        .collect()) // alloc: once a proc-macro crate, its macros
}

/// Whether `calling` enables `forced_keywords`, read without its def map: its proc macros
/// expand while that is built.
fn forced_keywords(db: &Db, calling: Crate) -> bool {
    UnstableFeatures::of_root(db, calling).forced_keywords
}

/// What a call of a macro of `kind` was given, as an error about it says it: the item a derive
/// or attribute is on (`struct Config`, `fn main`), found by its keyword among the input's
/// outermost tokens past its attributes and visibility, or a bang macro's input by its length.
fn input_item(kind: ProcMacroKind, input: &tt::TopSubtree) -> String {
    const ITEMS: [&str; 11] = ["struct", "enum", "union", "fn", "trait", "impl", "mod", "type", "const", "static", "use"];
    let keyword = |it: &tt::TtElement<'_>| match it {
        tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => ITEMS.iter().find(|k| **k == ident.sym.as_str()).copied(),
        _ => None,
    };
    if kind != ProcMacroKind::Bang {
        let mut tokens = input.iter().peekable();
        while let Some(token) = tokens.next() {
            let Some(mut said) = keyword(&token) else { continue };
            // `const fn`, `unsafe impl`: the last keyword is the item's.
            while let Some(next) = tokens.peek().and_then(keyword) {
                said = next;
                tokens.next();
            }
            return match tokens.next() {
                // alloc: error path
                Some(tt::TtElement::Leaf(tt::Leaf::Ident(name))) => format!("`{said} {}`", name.sym.as_str()),
                // alloc: error path
                _ => format!("an item `{said} ...`"),
            };
        }
    }
    // alloc: error path
    format!("an input of {} token trees", input.iter().count())
}

/// A macro as an expansion's span names it: `m::answer`, its crate then its name.
struct MacroPath<'a>(&'a Evaluated);

impl fmt::Display for MacroPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.0.library.name, self.0.name.as_str())
    }
}

/// What an expansion was given, as [`input_item`] says it, for its span.
struct InputItem<'a>(ProcMacroKind, &'a tt::TopSubtree);

impl fmt::Display for InputItem<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&input_item(self.0, self.1))
    }
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
        let ran = match &library.engine {
            Engine::Evaluator(bridge) => run::run(db, &what, bridge, call.env, preset, &expansions, &mut spent),
            Engine::Wasm(module) => module.run(db, call.env, self.index, &expansions, &mut spent),
        };
        run::ran_on(span, &spent, &ran);
        // An expansion that stopped (a budget, a refused call) names what it expanded and where,
        // beside the macro and what stopped it: a step limit says the steps it ran.
        ran.map_err(|stop| {
            let item = input_item(self.kind, call.subtree);
            system(format_args!("{}, expanding {item} in `{calling}`, {stop}", self.what()))
        })
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
        let library = &*self.library;
        // One span per expansion, kept or run: which macro, on what, by which engine. The
        // fields format only when a subscriber records them.
        let _span = tracing::info_span!(
            target: "blaze",
            "proc macro expansion",
            r#macro = %MacroPath(self),
            input = %InputItem(self.kind, call.subtree),
            engine = match library.engine {
                Engine::Evaluator(_) => "evaluator",
                Engine::Wasm(_) => "wasm",
            },
        )
        .entered();
        let keeping: &Keeping = match &library.macros.keeps {
            Keeps::In(keeping) => keeping,
            Keeps::Nothing(why) => {
                let ran = self.run(db, calling, &call)?;
                report_reads(ExpansionReads::Unknown(*why));
                return self.output(&ran, call.call_site);
            }
        };
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
        // A module's expansion is kept apart from the evaluator's, under the module it ran.
        if let Engine::Wasm(module) = &library.engine {
            given.part("wasm module", &module.id().0);
        }
        let key = RunKey { code: library.code.get(db, library.krate), given: given.finish() };
        let mut inputs = Calling::new(call.env);
        // What its key, reads and shape are written against: the calling crate's package and
        // `OUT_DIR` (`proc_macro_api::kept::roots`).
        let roots = proc_macro_api::kept::roots(call.env);
        let mut found = None;
        if stateless && library.seen.verdict() == Verdict::Stateless {
            let guard = blaze_cas::enter();
            let mut rooted = blaze_usercode::roots::Rooted::new(roots, &mut inputs);
            match index::find_reads(&keeping.expansions.store, &guard, &key, &mut rooted) {
                Ok(Found::Hit { value, shape, values: Some(values), verified, .. }) => {
                    let reads = match verified {
                        // alloc: what the query reporting it owns
                        None => ExpansionReads::Traced { shape: shape.into(), values },
                        // Verified across checkouts: what it reports changes once it is
                        // retracted, so what was filed by it is made again; a warm front
                        // end is not lowered into again once a record is published
                        // (`Keeping::retractions_now`).
                        Some(unmarked) => {
                            let depends = index::depends_key(&key, &unmarked);
                            let (shape, values) = proc_macro_api::kept::verified_reads(&depends, shape, values);
                            ExpansionReads::Traced { shape: shape.into(), values }
                        }
                    };
                    match keeping.mode {
                        Mode::Keep => {
                            let output = read(value, &spans).map_err(|why| system(format_args!("{}: {why}", self.what())))?;
                            report_reads(reads);
                            return Ok(output);
                        }
                        Mode::Check => found = Some((checked(value), reads, verified)),
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
                Err(FindError::TooManyShapes { run }) => {
                    return Err(system(format_args!(
                        "{}: run {run:?} has more than {} shapes",
                        self.what(),
                        index::MOST_SHAPES
                    )));
                }
            }
        }
        let ran = self.run(db, calling, &call)?;
        let output = self.output(&ran, call.call_site)?;
        let bytes = written(&output, &spans);
        // A check that found a value verified across checkouts expanding otherwise: the
        // record that retracts it is put (durable before the job is done), the expansion is
        // kept here alone below, and the check's error is what every way out gives.
        let mut retracted = false;
        if let Some((kept, reads, verified)) = found {
            if bytes.as_deref().map(checked) == Some(kept) {
                report_reads(reads);
                return Ok(output);
            }
            let Some(unmarked) = verified else {
                return Err(system(format_args!(
                    "{}: {}=check: it expanded otherwise than the store kept it, under a key of \
                     every input it was traced to read",
                    self.what(),
                    proc_macro_api::kept::CACHE
                )));
            };
            let record = verify::retraction(&key, &unmarked);
            keeping.put(Put { keep: Keep::Durable { run: record.run, traced: record.traced }, value: record.value, what: self.what() });
            retracted = true;
        }
        let done = |output: tt::TopSubtree| match retracted {
            false => Ok(output),
            true => Err(system(format_args!(
                "{}: {}=check: it expanded otherwise than the expansion verified across checkouts \
                 for the same inputs: it depends on where its roots are, so it is recorded as doing \
                 so and no checkout is given that expansion again",
                self.what(),
                proc_macro_api::kept::CACHE
            ))),
        };
        let unknown = |why: &'static str| {
            report_reads(ExpansionReads::Unknown(why));
        };
        if ran.asked {
            unknown("it asked the database");
            return done(output);
        }
        if !stateless || !ran.statics.is_empty() || library.seen.verdict() == Verdict::Stateful {
            unknown("it ran with state its calls before it left");
            return done(output);
        }
        if !ran.emitted.is_empty() {
            unknown("it emitted a diagnostic");
            return done(output);
        }
        if ran.unkeyable.is_some() {
            unknown("what it read can't be keyed");
            return done(output);
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
            return done(output);
        }
        // Kept and reported written against the crate's roots, marked with every one, and
        // given to another checkout only once a second one expanded it the same
        // (`blaze_usercode::roots::verify`). No copy for another to check when the output,
        // a variable it read or a string its key was given still holds a root in any spelling
        // (a literal path the macro made), it read outside its roots and the machine-wide
        // places, the cargo home's registry and git checkouts among them, or a root's links
        // moved since the memo was made (then it is made again).
        let kept = library.kept_roots(db, calling, roots, keeping.expansions.environment.passed());
        let view = kept.map.view();
        let marked = view.kept_bytes(&traced, bytes.as_deref(), proc_macro_api::kept::left(&keyed, &view));
        if marked.relinked {
            library.kept_roots.pin().remove(&calling);
        }
        let Some(value) = bytes else {
            // Nothing is kept: what it read is reported as this checkout's.
            report_reads(ExpansionReads::Traced {
                shape: index::Shape::of(&marked.local).encode().into(), // alloc: once, as an expansion is kept
                values: index::VALUES.traced(&marked.local),
            });
            return done(output);
        };
        let puts = match retracted {
            // Retracted: kept here alone, as no checkout is given what was verified (the
            // record is not in the store yet, so [`verify::keeps`] would not see it).
            // alloc: once, its one put
            true => vec![verify::Put { run: key, traced: marked.local, value, durable: false }],
            false => {
                let guard = blaze_cas::enter();
                let store = &keeping.expansions.store;
                let same = |theirs: &[u8], ours: &[u8]| -> Result<Option<String>, String> {
                    Ok((theirs != ours).then(|| "its output differs".to_owned())) // alloc: a difference said
                };
                let keeps = verify::keeps(|key| Ok(store.get(&guard, key)?.map(|it| it.bytes)), &view, &key, marked, value, same);
                keeps.map_err(|error| system(format_args!("{}: keeping it: {error:?}", self.what())))?.0
            }
        };
        // What it reports is the put made under its own key: here, marked with every root, or
        // verified, with the roots that were the same in every checkout compared, and then
        // also whether it was retracted since ([`proc_macro_api::kept::verified_reads`]).
        if let Some(it) = puts.iter().find(|it| it.run == key) {
            // alloc: once, as an expansion is kept
            let shape = index::Shape::of(&it.traced).encode();
            let values = index::VALUES.traced(&it.traced);
            let reads = match index::split_verified(&it.value) {
                None => ExpansionReads::Traced { shape: shape.into(), values },
                Some((unmarked, _)) => {
                    let (shape, values) = proc_macro_api::kept::verified_reads(&index::depends_key(&key, &unmarked), &shape, values);
                    ExpansionReads::Traced { shape: shape.into(), values }
                }
            };
            report_reads(reads);
        }
        // A record that a value verified across checkouts depends on where its roots are (another
        // checkout's copy expanded otherwise) is durable before the job is done: a crash
        // before would forget it, and the expansion would be verified and given again.
        for it in puts {
            keeping.put(Put::of(it, self.what()));
        }
        done(output)
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
