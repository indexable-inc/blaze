//! The deprecation and stability of a definition, as hir-def reads and inherits them
//! (`hir_def::attrs::stability`: rustc's `lookup_deprecation_entry` and `lookup_stability`),
//! and whether a macro's expansion may use an unstable feature.
//!
//! hir-def parses a definition's own attributes once, only when its attribute flags say it has
//! one, and keeps what it inherits as the definition it comes from: a lookup is a memo hit and
//! allocates nothing. The index adds what one check of a crate asks again and again: whether a
//! crate is staged, and whether this crate enables a feature, each asked once.
//!
//! Follows `compiler/rustc_passes/src/stability.rs` (`lookup_deprecation_entry`,
//! `lookup_stability`, `eval_stability`), `compiler/rustc_middle/src/middle/stability.rs`
//! (`DeprecationEntry`, `same_origin`) and `compiler/rustc_span/src/lib.rs`
//! (`Span::allows_unstable`) (MIT or Apache-2.0).

use std::hash::BuildHasher;

use blaze_hash::{FixedState, HashMap};
use blaze_mem::Frame;
use hashbrown::HashTable;
use hir::{HirFileId, InFile};
use hir_def::{
    AttrDefId, MacroId,
    attrs::stability::{
        self, Deprecation, DeprecationEntry, RustcVersion, Stability, StabilityDef, StabilityLevel,
    },
};
use intern::{Symbol, sym};
use syntax::{
    SyntaxNodePtr, TextRange,
    view::{AstNode as _, Node},
};

use super::{
    attr,
    def::{Def, ItemLike},
};
use crate::cx::Cx;

/// The index's table lives in the check's frame and goes with it.
pub(super) struct Index<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    staged: HashMap<hir::Crate, bool>,
    enabled: HashTable<(Symbol, bool), &'b Frame>,
    /// `RustcVersion::CURRENT` of this crate's graph; none when it names no toolchain.
    pub(super) current: Option<RustcVersion>,
}

impl<'a, 'b, 'db> Index<'a, 'b, 'db> {
    pub(super) fn new(cx: &'a Cx<'b, 'db>) -> Self {
        Index {
            cx,
            staged: HashMap::default(),
            enabled: HashTable::new_in(cx.frame()),
            current: RustcVersion::current(cx.db, cx.krate.base()),
        }
    }

    /// Whether `krate` is compiled with `#![feature(staged_api)]`.
    pub(super) fn staged(&mut self, krate: hir::Crate) -> bool {
        let db = self.cx.db;
        *self.staged.entry(krate).or_insert_with(|| krate.is_unstable_feature_enabled(db, &sym::staged_api))
    }

    /// `lookup_deprecation_entry` of `def`.
    pub(super) fn deprecation(&self, def: Def) -> Option<DeprecationEntry<'db>> {
        def.deprecation(self.cx.db)
    }

    /// `lookup_deprecation_entry` of the item-like of this crate at `ptr`.
    pub(super) fn local_deprecation(&self, ptr: InFile<SyntaxNodePtr>) -> Option<DeprecationEntry<'db>> {
        let node = self.cx.syntax_view(ptr);
        ItemLike::at(self.cx, node)?.deprecation(self.cx.db)
    }

    /// The stability of `def` rustc checks a use of (`eval_stability`): of a public definition
    /// of another crate.
    pub(super) fn foreign_stability(&self, def: Def) -> Option<&'db Stability> {
        let db = self.cx.db;
        (!self.is_local(def) && def.is_public(db)).then(|| def.stability(db)).flatten()
    }

    /// Whether `def` is of this crate: then so is every parent segment of a path to it.
    pub(super) fn is_local(&self, def: Def) -> bool {
        def.krate(self.cx.db) == Some(self.cx.krate)
    }

    /// A macro's own `#[deprecated]` (`SyntaxExtension::deprecation`: nothing inherited).
    pub(super) fn macro_deprecation(&self, mac: hir::Macro) -> Option<&'db Deprecation> {
        stability::own_deprecation(self.cx.db, StabilityDef::Def(AttrDefId::MacroId(MacroId::from(mac))))
    }

    /// A macro's own stability when it is unstable (`SyntaxExtension::stability`), of a crate
    /// with `staged_api`, this one or another.
    pub(super) fn macro_stability(&self, mac: hir::Macro) -> Option<&'db Stability> {
        stability::own_stability(self.cx.db, StabilityDef::Def(AttrDefId::MacroId(MacroId::from(mac))))
            .filter(|it| it.is_unstable())
    }

    /// Whether `eval_stability` denies a use at `range` in `file` of what is `stability`:
    /// unstable, its feature and the one implying it not enabled, nor allowed by the macro the
    /// use comes from. Checked cheapest first; nothing is cloned.
    pub(super) fn denied(&mut self, stability: &Stability, file: HirFileId, range: TextRange) -> bool {
        let StabilityLevel::Unstable { implied_by, .. } = &stability.level else { return false };
        let Index { cx, enabled, .. } = self;
        let cx = *cx;
        let feature = &stability.feature;
        !(enabled_in(cx, enabled, feature)
            || implied_by.as_ref().is_some_and(|it| enabled_in(cx, enabled, it))
            || allows_unstable(cx, file, range, feature))
    }

    /// `check_stability_and_deprecation`'s test of a macro's `stability`: denied unless its
    /// feature, or the one implying it, is enabled or allowed where the macro is used.
    pub(super) fn macro_denied(&mut self, stability: &Stability, file: HirFileId, range: TextRange) -> bool {
        let StabilityLevel::Unstable { implied_by, .. } = &stability.level else { return false };
        let Index { cx, enabled, .. } = self;
        let cx = *cx;
        let mut allowed = |it: &Symbol| enabled_in(cx, enabled, it) || allows_unstable(cx, file, range, it);
        !(allowed(&stability.feature) || implied_by.as_ref().is_some_and(|it| allowed(it)))
    }

    /// The message of `def`'s `#[rustc_allowed_through_unstable_modules]` when it is of a
    /// dependency and stable with one (`lookup_stability`, public or not).
    pub(super) fn through(&self, def: Def) -> Option<&'db Symbol> {
        if self.is_local(def) {
            return None;
        }
        match &def.stability(self.cx.db)?.level {
            StabilityLevel::Stable { allowed_through_unstable_modules, .. } => allowed_through_unstable_modules.as_ref(),
            StabilityLevel::Unstable { .. } => None,
        }
    }

    /// `lookup_stability` of `def`, of this crate or another, public or not.
    pub(super) fn stability(&self, def: Def) -> Option<&'db Stability> {
        def.stability(self.cx.db)
    }

    /// `lookup_stability` of the item-like of this crate at `ptr`.
    pub(super) fn local_stability(&self, ptr: InFile<SyntaxNodePtr>) -> Option<&'db Stability> {
        let node = self.cx.syntax_view(ptr);
        ItemLike::at(self.cx, node)?.stability(self.cx.db)
    }
}

/// Whether this crate enables `feature`, kept in `enabled`. A hit clones no `Symbol` (no
/// shared refcount written).
fn enabled_in(cx: &Cx<'_, '_>, enabled: &mut HashTable<(Symbol, bool), &Frame>, feature: &Symbol) -> bool {
    let hash = FixedState.hash_one(feature);
    if let Some((_, it)) = enabled.find(hash, |(it, _)| it == feature) {
        return *it;
    }
    let it = cx.krate.is_unstable_feature_enabled(cx.db, feature);
    // clone: the miss, once per distinct feature a use asks about; the table keeps its own `Symbol`
    enabled.insert_unique(hash, (feature.clone(), it), |(it, _)| FixedState.hash_one(it));
    it
}

/// `Span::allows_unstable(feature)` of `range` in `file`: the macro whose expansion its first
/// token comes from is defined with `#[allow_internal_unstable(feature)]`. A builtin macro's
/// expansion is its own, not the one rustc checks with the features its definition allows,
/// so it may use any.
fn allows_unstable(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange, feature: &Symbol) -> bool {
    use hir_expand::MacroDefKind;
    let db = cx.db;
    let ctxt = file.span_map(db).span_for_range(range).ctx;
    let Some(call) = ctxt.outer_expn(db) else { return false };
    let def = hir_expand::MacroCallId::from(call).loc(db).def;
    let node: Node<'_> = match def.kind {
        MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => match def.ast_id() {
            either::Either::Left(id) => id.to_view(db).syntax(),
            either::Either::Right(id) => id.to_view(db).syntax(),
        },
        MacroDefKind::BuiltIn(..)
        | MacroDefKind::BuiltInAttr(..)
        | MacroDefKind::BuiltInDerive(..)
        | MacroDefKind::BuiltInEager(..)
        | MacroDefKind::UnimplementedBuiltIn(..) => return true,
    };
    attr::allows_internal_unstable(node, hir::Crate::from(def.krate).cfg(db), feature)
}
