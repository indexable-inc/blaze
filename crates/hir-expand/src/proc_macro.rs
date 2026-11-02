//! Proc Macro Expander stuff

use core::fmt;
use std::any::Any;
use std::{panic::RefUnwindSafe, sync};

use base_db::{Crate, CrateBuilderId, CratesIdMap, Db, Env, ProcMacroLoadingError};
use intern::Symbol;
use blaze_hash::HashMap;
use blaze_query::db::{Durability, Setter};
use span::Span;
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
    /// The calling crate's environment variables.
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

    /// The content ID of the library the macro is code of: what the key of a crate that
    /// uses it covers (`hir_def::deps`). `None` when it is not known, which keeps the crates
    /// that use the macro out of dependency images.
    fn code(&self) -> Option<base_db::ContentId> {
        None
    }

    /// Run the expander on one call.
    fn expand(
        &self,
        db: &Db,
        call: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError>;

    /// Run in the calling crate's compilation session. Native expanders use `expand`.
    fn expand_in(
        &self,
        db: &Db,
        _calling_crate: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        self.expand(db, call)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool; // dyn: comparing expanders when a crate's macros are set, not per expansion
}

impl PartialEq for dyn ProcMacroExpander { // dyn: comparing expanders when a crate's macros are set, not per expansion
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
/// traced it: what the image of the calling crate is filed by (`hir_def::deps`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpansionReads {
    /// Nothing else.
    Nothing,
    /// The names it read, as the server encodes them, and the hash of the values they had.
    Traced { shape: sync::Arc<[u8]>, values: base_db::ContentId },
    /// Not known, and why: the calling crate gets no image.
    Unknown(&'static str),
}

thread_local! {
    static READS: std::cell::Cell<Option<ExpansionReads>> = const { std::cell::Cell::new(None) };
}

// Each access to `READS` is a call of its own, never inlined: an expansion that waits between
// two may resume on another thread, and an inlined access may reuse the address of the first
// thread's slot (docs/v2/plan/lanes/may-suspend.md).

/// What the expansion running on this thread read: called by an expander before `expand`
/// returns. An expander that does not call it has reads [`ExpansionReads::Unknown`].
#[inline(never)]
pub fn report_reads(reads: ExpansionReads) {
    carry_reads();
    READS.set(Some(reads));
}

/// Carries [`READS`] with each job on a fiber across its waits ([`blaze_query::carry`]): an
/// expansion that reported its reads and then waits on a query finds them, wherever it resumes.
fn carry_reads() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried_cell(&READS, Option::is_none),
            put: |it| stdx::lend::put_carried_cell(&READS, Option::is_none, it),
        });
    });
}

/// What the expander reported since the last call, cleared.
#[inline(never)]
pub(crate) fn take_reads() -> Option<ExpansionReads> {
    READS.take()
}

pub type ProcMacroLoadResult = Result<Vec<ProcMacro>, ProcMacroLoadingError>;
type StoredProcMacroLoadResult = Result<Box<[ProcMacro]>, ProcMacroLoadingError>;

#[derive(Default, Debug)]
pub struct ProcMacrosBuilder(HashMap<CrateBuilderId, Arc<CrateProcMacros>>);

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
            .collect::<HashMap<_, _>>();
        map.shrink_to_fit();
        ProcMacros::try_get(db)
            .unwrap_or_else(|| ProcMacros::new(db, Default::default()))
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
    pub by_crate: HashMap<Crate, Arc<CrateProcMacros>>,
}

impl ProcMacros {
    pub fn init_default(db: &mut Db, durability: Durability) {
        _ = Self::builder(Default::default()).durability(durability).new(db);
    }

    /// Gives each crate of `given` its proc macros (or why it has none) in place of those it
    /// had, as loading its dylib does, and notes it ([`Db::note_proc_macros`]): the one way to
    /// change a crate's list once the crate graph is set. A crate's def map lists its proc
    /// macros' expanders, so one resolved before its list was given must resolve again, and a
    /// set that is not noted leaves it naming none.
    pub fn give(db: &mut Db, given: impl IntoIterator<Item = (Crate, ProcMacroLoadResult)>) {
        db.batch(|db| {
            let all = ProcMacros::try_get(db).unwrap_or_else(|| ProcMacros::new(db, Default::default()));
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
    let name = krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str());
    let name = name.unwrap_or("<unnamed>");
    let why = match ProcMacros::get_for_crate(db, krate).map(|it| &it.0) {
        // alloc: an error's message, once per macro call that can't be expanded
        None => format!("the proc-macro crate `{name}` ({krate:?}) has no entry in the proc-macro map"),
        // alloc: as above
        Some(Err(e)) => format!("the proc-macro crate `{name}` ({krate:?}): {e}"),
        // alloc: as above
        Some(Ok(_)) => format!("the proc-macro crate `{name}` ({krate:?}) loaded and exports no macro of this name"),
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

    /// Whether this resolved macro shares mutable guest state with earlier calls.
    pub fn requires_session(self, db: &Db, def_crate: Crate) -> bool {
        ProcMacros::get_for_crate(db, def_crate)
            .and_then(|it| it.macros())
            .and_then(|it| it.get(self.proc_macro_id as usize))
            .is_some_and(|it| it.expander.requires_session())
    }

    pub fn as_expand_error(&self, db: &Db, def_crate: Crate) -> Option<ExpandErrorKind> {
        match self.proc_macro_id {
            Self::PROC_MACRO_ATTR_DISABLED => Some(ExpandErrorKind::ProcMacroAttrExpansionDisabled),
            Self::DISABLED_ID => Some(ExpandErrorKind::MacroDisabled),
            Self::MISSING_EXPANDER => Some(missing(db, def_crate)),
            _ => None,
        }
    }

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
    ) -> ExpandResult<tt::TopSubtree> {
        match self.proc_macro_id {
            Self::PROC_MACRO_ATTR_DISABLED => ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan { open: call_site, close: call_site }),
                ExpandError::new(call_site, ExpandErrorKind::ProcMacroAttrExpansionDisabled),
            ),
            Self::MISSING_EXPANDER => ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan { open: call_site, close: call_site }),
                ExpandError::new(call_site, missing(db, def_crate)),
            ),
            Self::DISABLED_ID => ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan { open: call_site, close: call_site }),
                ExpandError::new(call_site, ExpandErrorKind::MacroDisabled),
            ),
            id => {
                let proc_macros = match ProcMacros::get_for_crate(db, def_crate) {
                    Some(it) => it,
                    None => {
                        return ExpandResult::new(
                            tt::TopSubtree::empty(tt::DelimSpan {
                                open: call_site,
                                close: call_site,
                            }),
                            ExpandError::other(
                                call_site,
                                "internal error: no proc macros for crate",
                            ),
                        );
                    }
                };
                let proc_macro = match proc_macros.get(id, call_site) {
                    Ok(proc_macro) => proc_macro,
                    Err(e) => {
                        return ExpandResult::new(
                            tt::TopSubtree::empty(tt::DelimSpan {
                                open: call_site,
                                close: call_site,
                            }),
                            e,
                        );
                    }
                };

                // Proc macros have access to the environment variables of the invoking crate.
                let env = calling_crate.env(db);
                let call = ProcMacroCall {
                    subtree: tt,
                    attrs: attr_arg,
                    env,
                    def_site,
                    call_site,
                    mixed_site,
                    current_dir: calling_crate.data(db).proc_macro_cwd.as_str(),
                };

                match proc_macro.expander.expand_in(db, calling_crate, call) {
                    Ok(t) => ExpandResult::ok(t),
                    Err(err) => match err {
                        // Don't discard the item in case something unexpected happened while expanding attributes
                        ProcMacroExpansionError::System(text)
                            if proc_macro.kind == ProcMacroKind::Attr =>
                        {
                            ExpandResult {
                                value: tt.copy(), // alloc: error path: an attribute macro that failed keeps its item
                                err: Some(ExpandError::other(call_site, text)),
                            }
                        }
                        ProcMacroExpansionError::System(text)
                        | ProcMacroExpansionError::Panic(text) => ExpandResult::new(
                            tt::TopSubtree::empty(tt::DelimSpan {
                                open: call_site,
                                close: call_site,
                            }),
                            ExpandError::new(
                                call_site,
                                ExpandErrorKind::ProcMacroPanic(text.into_boxed_str()),
                            ),
                        ),
                    },
                }
            }
        }
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
        assert!(unbuilt.contains("`pm`") && unbuilt.contains("proc-macro not yet built"), "{unbuilt}");
        set(fixture.db_mut(), krate, Ok(Vec::new()));
        let loaded = why(fixture.db(), krate);
        assert!(loaded.contains("exports no macro of this name"), "{loaded}");
        let error = ExpandErrorKind::MissingProcMacroExpander { krate, why: unbuilt.clone() };
        let rendered = error.render_to_string(fixture.db());
        assert_eq!((rendered.message.as_str(), rendered.error), (&*unbuilt, true));
    }
}
