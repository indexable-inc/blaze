//! Proc Macro Expander stuff

use core::fmt;
use std::any::Any;
use std::{panic::RefUnwindSafe, sync};

use base_db::{Crate, CrateBuilderId, CratesIdMap, Db, Env, ProcMacroLoadingError};
use blaze_hash::HashMap;
use blaze_query::db::{Durability, Setter};
use intern::Symbol;
use span::Span;
use std::alloc::Global;
use triomphe::Arc;

use crate::{ExpandError, ExpandErrorKind, ExpandResult, tt};

#[derive(Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Debug, Hash)]
pub enum ProcMacroKind {
    CustomDerive,
    Bang,
    Attr,
}

/// One call of a proc macro: what every expander is given.
#[derive(Clone, Copy, Debug)]
pub struct ProcMacroCall<'a> {
    /// The input: the item for a derive or attribute, the arguments for a bang macro.
    pub subtree: &'a tt::TopSubtree,
    /// The attribute's own arguments, for [`ProcMacroKind::Attr`].
    pub attrs: Option<&'a tt::TopSubtree>,
    /// The calling crate's environment variables (`base_db::macro_env`).
    pub env: &'a Env,
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
    /// The calling crate's proc-macro working directory.
    pub current_dir: &'a str,
}

/// A proc-macro expander implementation.
pub trait ProcMacroExpander: fmt::Debug + Send + Sync + RefUnwindSafe + Any {
    /// Whether calls share mutable state within one calling crate's compilation.
    fn requires_session(&self) -> bool {
        false
    }

    /// Whether a calling crate's calls of this macro's crate run one at a time, in the order
    /// they expand: those of a crate that keeps a session, or whose state across calls is not
    /// known. Other crates' calls and calls of macros not ordered still run beside them.
    fn ordered(&self) -> bool {
        self.requires_session()
    }

    /// Whether [`Self::speculate`] runs calls from `calling_crate`'s session as it is now: the
    /// session ran a call in this revision and can give its state to another run. A batch asks
    /// once the first of its calls of this crate ran.
    fn speculates(&self, _db: &Db, _calling_crate: Crate) -> bool {
        false
    }

    /// Runs `call` ahead of its turn, from the state `calling_crate`'s session is in now, and
    /// keeps what it gave for [`Self::expand_in`] of the same call to take when its turn comes,
    /// if the session is then still in that state; otherwise that call runs as it would have.
    /// What any call gives is what it gives in order: a run ahead only saves time.
    fn speculate(&self, _db: &Db, _calling_crate: Crate, _call: ProcMacroCall<'_>) {}

    /// The content ID of the code the macro runs: its library's, and the compiled module it
    /// runs in when it runs in one, which the front end's code identity does not cover. What
    /// the key of a crate that uses it covers (`hir_def::deps`, the goal store's facts). `None`
    /// when it is not known, which keeps the crates that use the macro out of dependency images.
    fn code(&self) -> Option<base_db::ContentId> {
        None
    }

    /// Run the expander on one call: what it gave, with what it read and whether its session's
    /// state took part ([`Expanded`]), which every expander states for every call it expands.
    fn expand(&self, db: &Db, call: ProcMacroCall<'_>)
    -> Result<Expanded, ProcMacroExpansionError>;

    /// Run in the calling crate's compilation session. Native expanders use `expand`.
    fn expand_in(
        &self,
        db: &Db,
        _calling_crate: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        self.expand(db, call)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool; // dyn: comparing expanders when a crate's macros are set, not per expansion
}

impl PartialEq for dyn ProcMacroExpander {
    // dyn: comparing expanders when a crate's macros are set, not per expansion
    fn eq(&self, other: &Self) -> bool {
        self.eq_dyn(other)
    }
}

impl Eq for dyn ProcMacroExpander {} // dyn: comparing expanders when a crate's macros are set, not per expansion

#[derive(Debug)]
pub enum ProcMacroExpansionError {
    /// The proc-macro panicked.
    Panic(String),
    /// The server itself errored out.
    System(String),
}

/// What an expansion read besides its call and the variables it was given, as its server
/// traced it: what the image of the calling crate is filed by (`hir_def::deps`). An expander
/// states it for each call ([`Expanded::reads`]); nothing defaults it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpansionReads {
    /// Nothing else.
    Nothing,
    /// The names it read, as the server encodes them, and the hash of the values they had.
    Traced { shape: sync::Arc<[u8]>, values: base_db::ContentId },
    /// Not traced, and why, as the expander says it (it asked the database, it emitted a
    /// diagnostic, it read the clock, a test's macro that traces nothing): the calling crate
    /// gets no image.
    Unknown(&'static str),
}

/// Whether what an expansion gave is fixed by its call and what it read, or also by the state
/// its calling crate's session was in when it ran, which the calls before it left in the
/// expander, outside the database. An expander states it for each call ([`Expanded::state`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateUse {
    /// Fixed by its call and reads: a macro whose crate keeps no state, or a call of a crate
    /// that settles shown to give the same from the fresh state as from the settled one. Its
    /// expansion is reused while those are the same, whatever ran before it.
    Fixed,
    /// It may depend on the calls before it, and why. Its expansion runs again in every
    /// revision (an untracked read naming `why`), in the order its readers ask for it, and
    /// keeps its value when it gives the same.
    Session(&'static str),
}

/// What one call of an expander gave ([`ProcMacroExpander::expand`]).
#[derive(Debug)]
pub struct Expanded {
    pub tree: tt::TopSubtree,
    /// What it read besides its call.
    pub reads: ExpansionReads,
    /// Whether the session's state took part.
    pub state: StateUse,
}

impl Expanded {
    /// `tree`, from a call that read nothing but its call and kept no state.
    pub fn reads_nothing(tree: tt::TopSubtree) -> Expanded {
        Expanded { tree, reads: ExpansionReads::Nothing, state: StateUse::Fixed }
    }

    /// `tree`, from a call whose reads are not traced, for `why`, and that kept no state.
    pub fn untraced(tree: tt::TopSubtree, why: &'static str) -> Expanded {
        Expanded { tree, reads: ExpansionReads::Unknown(why), state: StateUse::Fixed }
    }
}

/// What the reads of a failed call are: none are kept for it, and its crate gets no image.
pub const FAILED_READS: &str = "it failed: what a failed expansion read is not kept";

/// What a failed call of a macro whose calls are ordered states of its session: its failure
/// may be of the state the calls before it left.
pub const FAILED_IN_SESSION: &str = "a failed call of a proc macro whose calls run in order: its failure may depend on the calls before it";

/// What a proc-macro call gave, from [`CustomProcMacroExpander::expand`]: the expansion, what
/// it read and whether its session's state took part, as its expander stated them, or as
/// [`FAILED_READS`] and [`FAILED_IN_SESSION`] state them for a call that failed.
#[derive(Debug)]
pub struct ProcMacroExpansion {
    pub result: ExpandResult<tt::TopSubtree>,
    pub reads: ExpansionReads,
    pub state: StateUse,
}

pub type ProcMacroLoadResult = Result<Vec<ProcMacro>, ProcMacroLoadingError>;
type StoredProcMacroLoadResult = Result<Box<[ProcMacro]>, ProcMacroLoadingError>;

#[derive(Default, Debug)]
pub struct ProcMacrosBuilder(HashMap<CrateBuilderId, Arc<CrateProcMacros>, Global>);

impl ProcMacrosBuilder {
    pub fn insert(
        &mut self,
        proc_macros_crate: CrateBuilderId,
        mut proc_macro: ProcMacroLoadResult,
    ) {
        if let Ok(proc_macros) = &mut proc_macro {
            // Sort proc macros to improve incrementality when only their order has changed (ideally the build system
            // will not change their order, but just to be sure).
            proc_macros.sort_unstable_by(|proc_macro, proc_macro2| {
                proc_macro
                    .name
                    .cmp_order(&proc_macro2.name)
                    .then_with(|| proc_macro.kind.cmp(&proc_macro2.kind))
            });
        }
        self.0.insert(
            proc_macros_crate,
            match proc_macro {
                Ok(it) => Arc::new(CrateProcMacros(Ok(it.into_boxed_slice()))), // alloc: once per crate, when its proc macros are loaded
                Err(e) => Arc::new(CrateProcMacros(Err(e))), // alloc: once per crate, when its proc macros are loaded
            },
        );
    }

    /// Builds [`ProcMacros`] and adds id to `db`
    pub(crate) fn build_in(self, db: &mut Db, crates_id_map: &CratesIdMap) {
        let mut map = self
            .0
            .into_iter()
            .map(|(krate, proc_macro)| (crates_id_map[&krate], proc_macro))
            .collect::<HashMap<_, _, Global>>();
        map.shrink_to_fit();
        ProcMacros::try_get(db)
            .unwrap_or_else(|| ProcMacros::new(db, Durability::HIGH, Default::default()))
            .set_by_crate(db)
            .with_durability(Durability::HIGH)
            .to(map);
        db.note_crates_changed();
    }
}

impl FromIterator<(CrateBuilderId, ProcMacroLoadResult)> for ProcMacrosBuilder {
    fn from_iter<T: IntoIterator<Item = (CrateBuilderId, ProcMacroLoadResult)>>(iter: T) -> Self {
        let mut builder = ProcMacrosBuilder::default();
        for (k, v) in iter {
            builder.insert(k, v);
        }
        builder
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CrateProcMacros(StoredProcMacroLoadResult);

/// The proc macros. Do not use [`Self::get`]! Use [`Self::get_for_crate`] instead.
#[blaze_query::input(singleton, debug)]
pub struct ProcMacros {
    #[returns(ref)]
    pub by_crate: HashMap<Crate, Arc<CrateProcMacros>, Global>,
}

impl ProcMacros {
    pub fn init_default(db: &mut Db, durability: Durability) {
        _ = Self::new(db, durability, Default::default());
    }

    /// Gives each crate of `given` its proc macros (or why it has none) in place of those it
    /// had, as loading its dylib does, and notes it ([`Db::note_proc_macros`]): the one way to
    /// change a crate's list once the crate graph is set. A crate's def map lists its proc
    /// macros' expanders, so one resolved before its list was given must resolve again, and a
    /// set that is not noted leaves it naming none.
    pub fn give(db: &mut Db, given: impl IntoIterator<Item = (Crate, ProcMacroLoadResult)>) {
        db.batch(base_db::Trigger::named("ProcMacros::give"), |db| {
            let all = ProcMacros::try_get(db)
                .unwrap_or_else(|| ProcMacros::new(db, Durability::HIGH, Default::default()));
            let mut by_crate = all.by_crate(db).clone();
            for (krate, macros) in given {
                by_crate.insert(krate, Arc::new(CrateProcMacros::new(macros))); // alloc: once per crate, when its proc macros are loaded
                db.note_proc_macros(krate);
            }
            all.set_by_crate(db).with_durability(Durability::HIGH).to(by_crate);
        });
    }
}

#[blaze_query::tracked]
impl ProcMacros {
    /// Incrementality query to prevent queries from directly depending on [`Self::get`].
    #[blaze_query::tracked(returns(as_ref), mem_tag = Expand)]
    pub fn get_for_crate(db: &Db, krate: Crate) -> Option<Arc<CrateProcMacros>> {
        Self::get(db).by_crate(db).get(&krate).cloned()
    }
}

impl CrateProcMacros {
    pub fn new(macros: ProcMacroLoadResult) -> Self {
        CrateProcMacros(macros.map(Vec::into_boxed_slice))
    }

    /// The macros of the crate, when they loaded.
    pub fn macros(&self) -> Option<&[ProcMacro]> {
        self.0.as_deref().ok()
    }

    fn get(&self, idx: u32, err_span: Span) -> Result<&ProcMacro, ExpandError> {
        let proc_macros = match &self.0 {
            Ok(proc_macros) => proc_macros,
            Err(_) => {
                return Err(ExpandError::other(
                    err_span,
                    "internal error: no proc macros for crate",
                ));
            }
        };
        proc_macros.get(idx as usize).ok_or_else(|| {
                ExpandError::other(err_span,
                    format!( // alloc: an error's message, on the error path
                        "internal error: proc-macro index out of bounds: the length is {} but the index is {}",
                        proc_macros.len(),
                        idx
                    )
                )
            }
        )
    }

    pub fn get_error(&self) -> Option<&ProcMacroLoadingError> {
        self.0.as_ref().err()
    }

    /// Fetch the [`CustomProcMacroExpander`]s and their corresponding names for the given crate.
    pub fn list(
        &self,
        def_site_ctx: span::SyntaxContext,
    ) -> Option<Box<[(crate::name::Name, CustomProcMacroExpander, bool)]>> {
        match &self.0 {
            Ok(proc_macros) => Some(
                proc_macros
                    .iter()
                    .enumerate()
                    .map(|(idx, it)| {
                        let name = crate::name::Name::new_symbol(it.name, def_site_ctx);
                        (name, CustomProcMacroExpander::new(idx as u32), it.disabled)
                    })
                    .collect(), // alloc: once per crate's def map: its proc macros' names
            ),
            _ => None,
        }
    }
}

/// Whether this crate can call a macro whose state belongs to a compilation session,
/// including macros reexported by a dependency.
#[blaze_query::tracked(returns(copy), mem_tag = Expand)]
pub fn may_have_session_macros(db: &Db, krate: Crate) -> bool {
    ProcMacros::get_for_crate(db, krate)
        .and_then(|it| {
            it.macros().map(|macros| macros.iter().any(|it| it.expander.requires_session()))
        })
        .unwrap_or(false)
        || krate.data(db).dependencies.iter().any(|dep| may_have_session_macros(db, dep.crate_id))
}

/// A loaded proc-macro.
#[derive(Debug, Clone, Eq)]
pub struct ProcMacro {
    /// The name of the proc macro.
    pub name: Symbol,
    pub kind: ProcMacroKind,
    /// The expander handle for this proc macro.
    pub expander: sync::Arc<dyn ProcMacroExpander>, // dyn: the edge to user code (native or wasm), one call per proc-macro expansion
    /// Whether this proc-macro is disabled for early name resolution. Notably, the
    /// [`Self::expander`] is still usable.
    pub disabled: bool,
}

// `#[derive(PartialEq)]` generates a strange "cannot move" error.
impl PartialEq for ProcMacro {
    fn eq(&self, other: &Self) -> bool {
        let Self { name, kind, expander, disabled } = self;
        let Self {
            name: other_name,
            kind: other_kind,
            expander: other_expander,
            disabled: other_disabled,
        } = other;
        name == other_name
            && kind == other_kind
            && expander == other_expander
            && disabled == other_disabled
    }
}

/// A custom proc-macro expander handle. This handle together with its crate resolves to a [`ProcMacro`]
/// Why a macro of proc-macro crate `krate` has no expander, as the proc-macro map says now:
/// what the call's error keeps, so it says the reason the expansion had even once the map
/// changes. An unexpanded macro fails the build whatever the reason.
fn missing(db: &Db, krate: Crate) -> ExpandErrorKind {
    let name = krate.named(db);
    let why = match ProcMacros::get_for_crate(db, krate).map(|it| &it.0) {
        // alloc: an error's message, once per macro call that can't be expanded
        None => {
            format!("the proc-macro crate `{name}` ({krate:?}) has no entry in the proc-macro map")
        }
        // alloc: as above
        Some(Err(e)) => format!("the proc-macro crate `{name}` ({krate:?}): {e}"),
        // alloc: as above
        Some(Ok(_)) => format!(
            "the proc-macro crate `{name}` ({krate:?}) loaded and exports no macro of this name"
        ),
    };
    ExpandErrorKind::MissingProcMacroExpander { krate, why: why.into_boxed_str() }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct CustomProcMacroExpander {
    proc_macro_id: u32,
}

impl CustomProcMacroExpander {
    const MISSING_EXPANDER: u32 = !0;
    const DISABLED_ID: u32 = !1;
    const PROC_MACRO_ATTR_DISABLED: u32 = !2;

    pub fn new(proc_macro_id: u32) -> Self {
        assert_ne!(proc_macro_id, Self::MISSING_EXPANDER);
        assert_ne!(proc_macro_id, Self::DISABLED_ID);
        assert_ne!(proc_macro_id, Self::PROC_MACRO_ATTR_DISABLED);
        Self { proc_macro_id }
    }

    /// An expander that always errors due to the actual proc-macro expander missing.
    pub const fn missing_expander() -> Self {
        Self { proc_macro_id: Self::MISSING_EXPANDER }
    }

    /// A dummy expander that always errors. This expander is used for macros that have been disabled.
    pub const fn disabled() -> Self {
        Self { proc_macro_id: Self::DISABLED_ID }
    }

    /// A dummy expander that always errors. This expander is used for attribute macros when
    /// proc-macro attribute expansion is disabled.
    pub const fn disabled_proc_attr() -> Self {
        Self { proc_macro_id: Self::PROC_MACRO_ATTR_DISABLED }
    }

    /// The place of the macro among those its crate's library lists, when it has one: not one
    /// missing or disabled.
    pub const fn listed(&self) -> Option<u32> {
        match self.proc_macro_id {
            Self::MISSING_EXPANDER | Self::DISABLED_ID | Self::PROC_MACRO_ATTR_DISABLED => None,
            id => Some(id),
        }
    }

    /// The macro-expander is missing or has yet to be build.
    pub const fn is_missing(&self) -> bool {
        self.proc_macro_id == Self::MISSING_EXPANDER
    }

    /// The macro is explicitly disabled and cannot be expanded.
    pub const fn is_disabled(&self) -> bool {
        self.proc_macro_id == Self::DISABLED_ID
    }

    /// The macro is explicitly disabled due to proc-macro attribute expansion being disabled.
    pub const fn is_disabled_proc_attr(&self) -> bool {
        self.proc_macro_id == Self::PROC_MACRO_ATTR_DISABLED
    }

    /// The macro this names among those `def_crate` loaded, when it has one.
    fn loaded(self, db: &Db, def_crate: Crate) -> Option<&ProcMacro> {
        ProcMacros::get_for_crate(db, def_crate)
            .and_then(|it| it.macros())
            .and_then(|it| it.get(self.proc_macro_id as usize))
    }

    /// Whether a calling crate's calls of `def_crate`'s macros run one at a time, in order
    /// ([`ProcMacroExpander::ordered`]).
    pub fn ordered(self, db: &Db, def_crate: Crate) -> bool {
        self.loaded(db, def_crate).is_some_and(|it| it.expander.ordered())
    }

    /// Whether calls of this macro from `calling_crate` can run ahead of their turn
    /// ([`ProcMacroExpander::speculates`]).
    pub fn speculates(self, db: &Db, def_crate: Crate, calling_crate: Crate) -> bool {
        self.loaded(db, def_crate).is_some_and(|it| it.expander.speculates(db, calling_crate))
    }

    /// Runs the call ahead of its turn ([`ProcMacroExpander::speculate`]), given what
    /// [`Self::expand`] gives it. Does nothing for a macro that is missing or disabled.
    #[allow(clippy::too_many_arguments)]
    pub fn speculate(
        self,
        db: &Db,
        def_crate: Crate,
        calling_crate: Crate,
        tt: &tt::TopSubtree,
        attr_arg: Option<&tt::TopSubtree>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
    ) {
        if self.listed().is_none() {
            return;
        }
        let Some(proc_macro) = self.loaded(db, def_crate) else { return };
        let call = Self::call(db, calling_crate, tt, attr_arg, def_site, call_site, mixed_site);
        proc_macro.expander.speculate(db, calling_crate, call);
    }

    /// What an expander is given for a call from `calling_crate`: [`Self::expand`]'s and
    /// [`Self::speculate`]'s alike, so a run ahead is given what the call's turn gives.
    fn call<'a>(
        db: &'a Db,
        calling_crate: Crate,
        tt: &'a tt::TopSubtree,
        attr_arg: Option<&'a tt::TopSubtree>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
    ) -> ProcMacroCall<'a> {
        // Proc macros have access to the environment variables of the invoking crate, and
        // `CARGO_PRIMARY_PACKAGE` when it is primary, as rustc's process has them.
        ProcMacroCall {
            subtree: tt,
            attrs: attr_arg,
            env: base_db::macro_env(db, calling_crate),
            def_site,
            call_site,
            mixed_site,
            current_dir: calling_crate.data(db).proc_macro_cwd.as_str(),
        }
    }

    pub fn as_expand_error(&self, db: &Db, def_crate: Crate) -> Option<ExpandErrorKind> {
        match self.proc_macro_id {
            Self::PROC_MACRO_ATTR_DISABLED => Some(ExpandErrorKind::ProcMacroAttrExpansionDisabled),
            Self::DISABLED_ID => Some(ExpandErrorKind::MacroDisabled),
            Self::MISSING_EXPANDER => Some(missing(db, def_crate)),
            _ => None,
        }
    }

    /// Runs the call, or says why it can't: a macro missing or disabled, or a failure of its
    /// expander, whose reads are [`FAILED_READS`]. A failed call of an expander whose calls run
    /// in order states its session as [`FAILED_IN_SESSION`]; any other failure is of what the
    /// database holds (the proc-macro map, the call), so it keeps no state.
    pub fn expand(
        self,
        db: &Db,
        def_crate: Crate,
        calling_crate: Crate,
        tt: &tt::TopSubtree,
        attr_arg: Option<&tt::TopSubtree>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
    ) -> ProcMacroExpansion {
        let empty = || tt::TopSubtree::empty(tt::DelimSpan { open: call_site, close: call_site });
        let refused = |err: ExpandError| ProcMacroExpansion {
            result: ExpandResult::new(empty(), err),
            reads: ExpansionReads::Unknown(FAILED_READS),
            state: StateUse::Fixed,
        };
        let id = match self.proc_macro_id {
            Self::PROC_MACRO_ATTR_DISABLED => {
                return refused(ExpandError::new(
                    call_site,
                    ExpandErrorKind::ProcMacroAttrExpansionDisabled,
                ));
            }
            Self::MISSING_EXPANDER => {
                return refused(ExpandError::new(call_site, missing(db, def_crate)));
            }
            Self::DISABLED_ID => {
                return refused(ExpandError::new(call_site, ExpandErrorKind::MacroDisabled));
            }
            id => id,
        };
        let Some(proc_macros) = ProcMacros::get_for_crate(db, def_crate) else {
            return refused(ExpandError::other(
                call_site,
                "internal error: no proc macros for crate",
            ));
        };
        let proc_macro = match proc_macros.get(id, call_site) {
            Ok(proc_macro) => proc_macro,
            Err(e) => return refused(e),
        };
        let call = Self::call(db, calling_crate, tt, attr_arg, def_site, call_site, mixed_site);
        let err = match proc_macro.expander.expand_in(db, calling_crate, call) {
            Ok(Expanded { tree, reads, state }) => {
                return ProcMacroExpansion { result: ExpandResult::ok(tree), reads, state };
            }
            Err(err) => err,
        };
        let result = match err {
            // Don't discard the item in case something unexpected happened while expanding attributes
            ProcMacroExpansionError::System(text) if proc_macro.kind == ProcMacroKind::Attr => {
                ExpandResult {
                    value: tt.copy(), // alloc: error path: an attribute macro that failed keeps its item
                    err: Some(ExpandError::other(call_site, text)),
                }
            }
            ProcMacroExpansionError::System(text) | ProcMacroExpansionError::Panic(text) => {
                ExpandResult::new(
                    empty(),
                    ExpandError::new(
                        call_site,
                        ExpandErrorKind::ProcMacroPanic(text.into_boxed_str()),
                    ),
                )
            }
        };
        let state = match proc_macro.expander.ordered() {
            true => StateUse::Session(FAILED_IN_SESSION),
            false => StateUse::Fixed,
        };
        ProcMacroExpansion { result, reads: ExpansionReads::Unknown(FAILED_READS), state }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{db, test_crate};

    fn why(db: &Db, krate: Crate) -> Box<str> {
        match missing(db, krate) {
            ExpandErrorKind::MissingProcMacroExpander { krate: of, why } => {
                assert_eq!(of, krate);
                why
            }
            other => panic!("not a missing expander: {other:?}"),
        }
    }

    fn set(db: &mut Db, krate: Crate, macros: ProcMacroLoadResult) {
        ProcMacros::give(db, [(krate, macros)]);
    }

    #[test]
    fn a_missing_expander_says_why_as_the_map_had_it_at_the_call() {
        let mut fixture = db("//- /lib.rs crate:pm\npub fn f() {}\n");
        let krate = test_crate(fixture.db());
        let none = why(fixture.db(), krate);
        assert!(none.contains("`pm`") && none.contains("no entry in the proc-macro map"), "{none}");
        set(fixture.db_mut(), krate, Err(ProcMacroLoadingError::NotYetBuilt));
        let unbuilt = why(fixture.db(), krate);
        assert!(
            unbuilt.contains("`pm`") && unbuilt.contains("proc-macro not yet built"),
            "{unbuilt}"
        );
        set(fixture.db_mut(), krate, Ok(Vec::new()));
        let loaded = why(fixture.db(), krate);
        assert!(loaded.contains("exports no macro of this name"), "{loaded}");
        let error = ExpandErrorKind::MissingProcMacroExpander { krate, why: unbuilt.clone() };
        let rendered = error.render_to_string(fixture.db());
        assert_eq!((rendered.message.as_str(), rendered.error), (&*unbuilt, true));
    }
}
