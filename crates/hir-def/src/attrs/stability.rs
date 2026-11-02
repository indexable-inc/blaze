//! The deprecation and the stability of a definition: what its own `#[deprecated]`,
//! `#[stable]` and `#[unstable]` say, else what it inherits from the definition it is in.
//!
//! An attribute rustc rejects as malformed gives nothing here (its error is the attribute
//! checks'), and the first well-formed one counts. A definition's own attributes are parsed
//! only when its [`AttrFlags`] say it has one. What a definition inherits is kept as the
//! definition it comes from ([`StabilityDef`], copied), so a module's deprecation is stored
//! once, not once per item in it.
//!
//! Follows `compiler/rustc_attr_parsing/src/attributes/{deprecation,stability,util}.rs` (the
//! parse), `compiler/rustc_passes/src/stability.rs` (`lookup_deprecation_entry`,
//! `lookup_stability`, `inherit_deprecation`, `inherit_stability`) and
//! `compiler/rustc_hir/src/{stability.rs,attrs/data_structures.rs}` (the types) (MIT or
//! Apache-2.0).

use std::{num::NonZero, ops::ControlFlow, sync::LazyLock};

use base_db::{Db, Crate, CrateOrigin, LangCrateOrigin};
use hir_expand::{AstId, HirFileId, InFile, Lookup, attrs::expand_cfg_attr_view, name::Name};
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use smallvec::SmallVec;
use span::{AstIdNode, ErasedFileAstId};
use syntax::{
    NodeOrToken, SyntaxKind, T,
    view::{self as ast, AstNode, AstToken, HasAttrs},
};

use super::{AttrFlags, Tree, attr_value, attrs_source, collect_attrs_in, for_each_field, is_one};
use crate::{
    AdtId, AssocItemId, AttrDefId, BlockIdLt, FieldId, HasModule, ItemContainerId, ModuleDefId,
    ModuleId, ModuleIdLt, VariantId,
    nameres::assoc::{ImplItems, TraitItems},
    unstable_features::UnstableFeatures,
};

/// A definition rustc's stability reads the attributes of: an item, an associated item, a
/// variant, a module, a macro or a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StabilityDef {
    Def(AttrDefId),
    Field(FieldId),
}

impl From<AttrDefId> for StabilityDef {
    fn from(def: AttrDefId) -> Self {
        StabilityDef::Def(def)
    }
}

impl From<FieldId> for StabilityDef {
    fn from(field: FieldId) -> Self {
        StabilityDef::Field(field)
    }
}

/// `RustcVersion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RustcVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl RustcVersion {
    /// `RustcVersion::CURRENT` for `krate`: the release of the rustc its graph is built as
    /// (`CrateWorkspaceData::toolchain`, `1.101.0` for `1.101.0-nightly`). None for a graph
    /// that names no toolchain.
    pub fn current(db: &Db, krate: Crate) -> Option<RustcVersion> {
        let version = krate.workspace_data(db).toolchain.as_ref()?;
        Some(RustcVersion {
            major: u16::try_from(version.major).ok()?,
            minor: u16::try_from(version.minor).ok()?,
            patch: u16::try_from(version.patch).ok()?,
        })
    }

    /// rustc's `parse_version`: `major.minor[.patch]`, nothing after a `-`.
    pub fn parse(text: &str) -> Option<RustcVersion> {
        let mut parts = text.split('-');
        let digits = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let mut digits = digits.splitn(3, '.');
        let major = digits.next()?.parse().ok()?;
        let minor = digits.next()?.parse().ok()?;
        let patch = digits.next().unwrap_or("0").parse().ok()?;
        Some(RustcVersion { major, minor, patch })
    }
}

/// `DeprecatedSince`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DeprecatedSince {
    RustcVersion(RustcVersion),
    /// `since = "TBD"`.
    Future,
    /// Any other `since`, in a crate without `#![feature(staged_api)]`.
    NonStandard(Symbol),
    /// No `since`, in a crate without `staged_api`.
    Unspecified,
    /// A `since` a staged crate can't have, or none. `CURRENT_RUSTC_VERSION` is this too in
    /// a graph that names no toolchain ([`RustcVersion::current`]).
    Err,
}

/// `Deprecation`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Deprecation {
    pub since: DeprecatedSince,
    pub note: Option<Symbol>,
    pub suggestion: Option<Symbol>,
}

impl Deprecation {
    /// `is_in_effect`: `deprecated` rather than `deprecated_in_future`, with `current` as
    /// `RustcVersion::CURRENT`.
    pub fn is_in_effect(&self, current: RustcVersion) -> bool {
        match self.since {
            DeprecatedSince::RustcVersion(since) => since <= current,
            DeprecatedSince::Future => false,
            DeprecatedSince::NonStandard(_)
            | DeprecatedSince::Unspecified
            | DeprecatedSince::Err => true,
        }
    }
}

/// `DeprecationEntry`: a deprecation and the definition whose attribute it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeprecationEntry<'db> {
    pub deprecation: &'db Deprecation,
    pub origin: StabilityDef,
}

impl DeprecationEntry<'_> {
    /// `same_origin`: both are the one attribute.
    pub fn same_origin(&self, other: &DeprecationEntry<'_>) -> bool {
        self.origin == other.origin
    }
}

/// `Stability`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Stability {
    pub level: StabilityLevel,
    pub feature: Symbol,
}

impl Stability {
    pub fn is_unstable(&self) -> bool {
        matches!(self.level, StabilityLevel::Unstable { .. })
    }
}

/// `StabilityLevel`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StabilityLevel {
    Unstable {
        reason: UnstableReason,
        /// `issue = "none"` is `None`.
        issue: Option<NonZero<u32>>,
        is_soft: bool,
        implied_by: Option<Symbol>,
        old_name: Option<Symbol>,
    },
    Stable {
        since: StableSince,
        /// `#[rustc_allowed_through_unstable_modules = ".."]`: the deprecation message a
        /// path to the item through an unstable module gets instead of an error.
        allowed_through_unstable_modules: Option<Symbol>,
    },
}

/// `UnstableReason`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UnstableReason {
    None,
    /// Of `-Zforce-unstable-if-unmarked`.
    Default,
    Some(Symbol),
}

impl UnstableReason {
    /// `to_opt_reason`: the reason written, or for `Default` rustc's
    /// `sym::unstable_location_reason_default`.
    pub fn to_opt_reason(&self) -> Option<&str> {
        match self {
            UnstableReason::None => None,
            UnstableReason::Default => Some(
                "this crate is being loaded from the sysroot, an unstable location; did you mean to \
                 load this crate from crates.io via `Cargo.toml` instead?",
            ),
            UnstableReason::Some(reason) => Some(reason.as_str()),
        }
    }
}

/// `StableSince`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StableSince {
    Version(RustcVersion),
    /// `since = "CURRENT_RUSTC_VERSION"`.
    Current,
    Err,
}

/// The stability `-Zforce-unstable-if-unmarked` gives everything of a crate without
/// `staged_api` (rustc's `FORCE_UNSTABLE`).
static FORCE_UNSTABLE: LazyLock<Stability> = LazyLock::new(|| Stability {
    level: StabilityLevel::Unstable {
        reason: UnstableReason::Default,
        issue: NonZero::new(27812),
        is_soft: false,
        implied_by: None,
        old_name: None,
    },
    feature: Symbol::intern("rustc_private"),
});

/// `lookup_deprecation_entry(def)`: its own `#[deprecated]`, else the one of the definition
/// it is in. rustc's `inherit_deprecation` holds for every definition here (only generic
/// parameters don't inherit).
pub fn deprecation_entry(
    db: &Db,
    def: StabilityDef,
) -> Option<DeprecationEntry<'_>> {
    let origin = deprecation_from(db, def)?;
    Some(DeprecationEntry { deprecation: own(db, origin)?.deprecation.as_ref()?, origin })
}

/// `lookup_stability(def)`: in a crate with `staged_api`, its own `#[stable]` or
/// `#[unstable]`, else the one of the definition it is in when that is unstable or `def`
/// is a field or a variant (`inherit_stability`). A crate without `staged_api` has none,
/// unless it is built with `-Zforce-unstable-if-unmarked`.
pub fn stability(db: &Db, def: StabilityDef) -> Option<&Stability> {
    match stability_from(db, def)? {
        StabilityFrom::Own(origin) => own(db, origin)?.stability.as_ref(),
        StabilityFrom::ForceUnstable => Some(&FORCE_UNSTABLE),
    }
}

/// `lookup_default_body_stability(def)`: what `def`'s own `#[rustc_default_body_unstable]` says
/// of its default body, in a crate with `staged_api` (`BodyStabilityParser`), nothing inherited.
pub fn default_body_stability(db: &Db, def: AttrDefId) -> Option<&Stability> {
    if !UnstableFeatures::query(db, def.krate(db)).staged_api {
        return None;
    }
    return default_body_of(db, def).as_ref();

    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    fn default_body_of(db: &Db, def: AttrDefId) -> Option<Stability> {
        let (source, outer_mod_decl, crate_attrs, krate) = attrs_source(db, def, Tree::Live);
        collect_attrs_in(db, source, outer_mod_decl, crate_attrs, krate, |meta, _| {
            match meta.path() {
                Some(path) if is_one(path, "rustc_default_body_unstable") => {
                    let ast::Meta::TokenTreeMeta(list) = meta else {
                        return ControlFlow::Break(None);
                    };
                    ControlFlow::Break(
                        list.token_tree().and_then(items).and_then(|it| parse_unstable(&it)),
                    )
                }
                _ => ControlFlow::Continue(()),
            }
        })
        .flatten()
    }
}

/// What `def`'s own `#[deprecated]` says, nothing inherited: rustc's
/// `SyntaxExtension::deprecation` of a macro.
pub fn own_deprecation(db: &Db, def: StabilityDef) -> Option<&Deprecation> {
    own(db, def)?.deprecation.as_ref()
}

/// What `def`'s own `#[stable]` or `#[unstable]` says in a crate with `staged_api`, nothing
/// inherited: rustc's `SyntaxExtension::stability` of a macro.
pub fn own_stability(db: &Db, def: StabilityDef) -> Option<&Stability> {
    own(db, def)?.stability.as_ref()
}

fn deprecation_from(db: &Db, def: StabilityDef) -> Option<StabilityDef> {
    return match def {
        StabilityDef::Def(def) => deprecation_origin(db, def),
        StabilityDef::Field(field) => {
            deprecation_or_parent(db, def, || Some(AttrDefId::from(field.parent).into()))
        }
    };

    #[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
    fn deprecation_origin(db: &Db, def: AttrDefId) -> Option<StabilityDef> {
        deprecation_or_parent(db, def.into(), || parent(db, def))
    }

    fn deprecation_or_parent(
        db: &Db,
        def: StabilityDef,
        parent: impl FnOnce() -> Option<StabilityDef>,
    ) -> Option<StabilityDef> {
        match own(db, def).and_then(|it| it.deprecation.as_ref()) {
            Some(_) => Some(def),
            None => deprecation_from(db, parent()?),
        }
    }
}

/// Where a definition's stability comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StabilityFrom {
    /// The attribute of this definition.
    Own(StabilityDef),
    ForceUnstable,
}

fn stability_from(db: &Db, def: StabilityDef) -> Option<StabilityFrom> {
    return match def {
        StabilityDef::Def(def) => stability_origin(db, def),
        StabilityDef::Field(field) => {
            let parent = || -> Option<StabilityDef> { Some(AttrDefId::from(field.parent).into()) };
            stability_or_parent(db, def, field.parent.krate(db), parent, true)
        }
    };

    #[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
    fn stability_origin(db: &Db, def: AttrDefId) -> Option<StabilityFrom> {
        let inherits = matches!(def, AttrDefId::EnumVariantId(_));
        stability_or_parent(db, def.into(), def.krate(db), || parent(db, def), inherits)
    }

    fn stability_or_parent(
        db: &Db,
        def: StabilityDef,
        krate: Crate,
        parent: impl FnOnce() -> Option<StabilityDef>,
        inherits: bool,
    ) -> Option<StabilityFrom> {
        if !UnstableFeatures::query(db, krate).staged_api {
            // rustc gives the crate root `FORCE_UNSTABLE` and each definition its parent's
            // when that is unstable: every definition here inherits, so each has it.
            return forces_unstable(db, krate).then_some(StabilityFrom::ForceUnstable);
        }
        if own(db, def).is_some_and(|it| it.stability.is_some()) {
            return Some(StabilityFrom::Own(def));
        }
        let from = stability_from(db, parent()?)?;
        let unstable = match from {
            StabilityFrom::Own(origin) => {
                own(db, origin)?.stability.as_ref().is_some_and(Stability::is_unstable)
            }
            StabilityFrom::ForceUnstable => true,
        };
        (unstable || inherits).then_some(from)
    }
}

/// Whether `krate` is built with `-Zforce-unstable-if-unmarked`: cargo's `-Zbuild-std` passes
/// it to every unit of the standard library's graph, the crates load-cargo gives a
/// `CrateOrigin::Lang` origin, and blaze refuses it anywhere else (`blaze-cargo`'s
/// `unstable.rs`). The flag only changes a crate without `staged_api`, and the real `core`,
/// `alloc`, `std`, `proc_macro` and `test` always have it: of the standard library's graph
/// only its dependencies can be unmarked, so a `core` without it (a test fixture's) is not
/// forced.
fn forces_unstable(db: &Db, krate: Crate) -> bool {
    matches!(
        krate.data(db).origin,
        CrateOrigin::Lang(LangCrateOrigin::Dependency | LangCrateOrigin::Other)
    )
}

/// What a definition's own attributes say.
#[derive(Debug, Default, PartialEq, Eq)]
struct Own {
    deprecation: Option<Deprecation>,
    stability: Option<Stability>,
}

const NONE: &Own = &Own { deprecation: None, stability: None };

/// The own attributes of `def`, parsed only when its flags say it has one of them.
fn own(db: &Db, def: StabilityDef) -> Option<&Own> {
    const READ: AttrFlags =
        AttrFlags::IS_DEPRECATED.union(AttrFlags::IS_STABLE).union(AttrFlags::IS_UNSTABLE);
    let own = match def {
        StabilityDef::Def(def) => {
            if !AttrFlags::query(db, def).intersects(READ) {
                return None;
            }
            own_of_def(db, def)
        }
        StabilityDef::Field(field) => {
            if !AttrFlags::query_field(db, field).intersects(READ) {
                return None;
            }
            own_of_fields(db, field.parent).get(u32::from(field.local_id.into_raw()) as usize)?
        }
    };
    (own != NONE).then_some(own)
}

#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
fn own_of_def(db: &Db, def: AttrDefId) -> Own {
    let (source, outer_mod_decl, crate_attrs, krate) = attrs_source(db, def, Tree::Live);
    let context = Context::of(db, krate);
    let mut reader = Reader::default();
    collect_attrs_in(db, source, outer_mod_decl, crate_attrs, krate, |meta, file| {
        reader.read(&context, meta, |value| attr_value(db, def, krate, InFile::new(file, value)));
        ControlFlow::<()>::Continue(())
    });
    reader.finish()
}

/// The own attributes of each field of `variant` its cfg leaves in, by `LocalFieldId`.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
fn own_of_fields(db: &Db, variant: VariantId) -> Box<[Own]> {
    let krate = variant.krate(db);
    let context = Context::of(db, krate);
    blaze_mem::frame::current(|frame| {
        let mut fields = Vec::new_in(frame);
        for_each_field(db, variant, Tree::Live, |_, cfg, field| {
            let mut reader = Reader::default();
            expand_cfg_attr_view(
                field.value.attrs(),
                || cfg,
                |meta, _| {
                    reader.read(&context, meta, |value| {
                        attr_value(db, variant.into(), krate, field.with_value(value))
                    });
                    ControlFlow::<()>::Continue(())
                },
            );
            fields.push(reader.finish());
        });
        // alloc: the memo's fields, exact, once per variant whose fields' stability is asked
        fields.drain(..).collect()
    })
}

/// What the parse of a crate's attributes depends on.
struct Context {
    /// `#![feature(staged_api)]`.
    staged: bool,
    current: Option<RustcVersion>,
}

impl Context {
    fn of(db: &Db, krate: Crate) -> Context {
        Context {
            staged: UnstableFeatures::query(db, krate).staged_api,
            current: RustcVersion::current(db, krate),
        }
    }
}

/// Reads one definition's attributes in order.
#[derive(Default)]
struct Reader {
    deprecation: Option<Deprecation>,
    stability: Option<Stability>,
    allowed_through_unstable_modules: Option<Symbol>,
}

impl Reader {
    fn read<'t>(
        &mut self,
        context: &Context,
        meta: ast::Meta<'t>,
        expand: impl FnOnce(ast::Expr<'t>) -> Option<Symbol>,
    ) {
        let Some(path) = meta.path() else { return };
        if is_one(path, "deprecated") {
            if self.deprecation.is_none() {
                self.deprecation = parse_deprecation(context, meta, expand);
            }
        } else if is_one(path, "stable") || is_one(path, "unstable") {
            // A stability attribute outside a staged crate is an error, and gives nothing.
            if context.staged
                && self.stability.is_none()
                && let ast::Meta::TokenTreeMeta(list) = meta
                && let Some(items) = list.token_tree().and_then(items)
            {
                self.stability = match is_one(path, "unstable") {
                    true => parse_unstable(&items),
                    false => parse_stable(&items),
                };
            }
        } else if is_one(path, "rustc_allowed_through_unstable_modules")
            && let ast::Meta::KeyValueMeta(meta) = meta
            && let Some(value) = meta.expr().and_then(string_literal)
        {
            self.allowed_through_unstable_modules = Some(value);
        }
    }

    fn finish(self) -> Own {
        let Reader { deprecation, mut stability, allowed_through_unstable_modules } = self;
        if let Some(Stability {
            level: StabilityLevel::Stable { allowed_through_unstable_modules: through, .. },
            ..
        }) = &mut stability
        {
            *through = allowed_through_unstable_modules;
        }
        Own { deprecation, stability }
    }
}

/// `DeprecationParser`: `#[deprecated]`, `#[deprecated = ".."]` or `#[deprecated(since =
/// "..", note = "..", suggestion = "..")]`. `expand` reads a key-value attribute's value, with
/// its macros expanded.
fn parse_deprecation<'t>(
    context: &Context,
    meta: ast::Meta<'t>,
    expand: impl FnOnce(ast::Expr<'t>) -> Option<Symbol>,
) -> Option<Deprecation> {
    let (mut since, mut note, mut suggestion) = (None, None, None);
    match meta {
        ast::Meta::PathMeta(_) => {}
        ast::Meta::KeyValueMeta(meta) => note = Some(expand(meta.expr()?)?),
        ast::Meta::TokenTreeMeta(meta) => {
            for (key, value) in items(meta.token_tree()?)? {
                let slot = match key_text(key) {
                    "since" => &mut since,
                    "note" => &mut note,
                    "suggestion" => &mut suggestion,
                    _ => return None,
                };
                insert(slot, &value)?;
            }
        }
        _ => return None,
    }
    let staged = context.staged;
    let since = match since {
        Some(since) if since.as_str() == "TBD" => DeprecatedSince::Future,
        Some(since) if !staged => DeprecatedSince::NonStandard(since),
        Some(since) if since.as_str() == "CURRENT_RUSTC_VERSION" => {
            context.current.map_or(DeprecatedSince::Err, DeprecatedSince::RustcVersion)
        }
        Some(since) => RustcVersion::parse(since.as_str())
            .map_or(DeprecatedSince::Err, DeprecatedSince::RustcVersion),
        None if staged => DeprecatedSince::Err,
        None => DeprecatedSince::Unspecified,
    };
    if staged && note.is_none() {
        return None;
    }
    Some(Deprecation { since, note, suggestion })
}

/// `parse_stability`: `#[stable(feature = "..", since = "..")]`.
fn parse_stable(items: &Items<'_>) -> Option<Stability> {
    let (mut feature, mut since) = (None, None);
    for (key, value) in items {
        let slot = match key_text(*key) {
            "feature" => &mut feature,
            "since" => &mut since,
            _ => return None,
        };
        insert(slot, value)?;
    }
    let feature = feature.filter(|it| is_ident(it.as_str()))?;
    let since = match since {
        Some(since) if since.as_str() == "CURRENT_RUSTC_VERSION" => StableSince::Current,
        Some(since) => {
            RustcVersion::parse(since.as_str()).map_or(StableSince::Err, StableSince::Version)
        }
        None => StableSince::Err,
    };
    Some(Stability {
        level: StabilityLevel::Stable { since, allowed_through_unstable_modules: None },
        feature,
    })
}

/// `parse_unstability`: `#[unstable(feature = "..", issue = "..", reason = "..", soft,
/// implied_by = "..", old_name = "..")]`.
fn parse_unstable(items: &Items<'_>) -> Option<Stability> {
    let (mut feature, mut reason, mut issue, mut implied_by, mut old_name) =
        (None, None, None, None, None);
    let mut is_soft = false;
    for (key, value) in items {
        let slot = match key_text(*key) {
            "feature" => &mut feature,
            "reason" => &mut reason,
            "issue" => &mut issue,
            "implied_by" => &mut implied_by,
            "old_name" => &mut old_name,
            // `soft = ..` is an error that still makes it soft.
            "soft" => {
                is_soft = true;
                continue;
            }
            _ => return None,
        };
        insert(slot, value)?;
    }
    let issue = match issue?.as_str() {
        "none" => None,
        issue => Some(issue.parse::<NonZero<u32>>().ok()?),
    };
    let feature = feature.filter(|it| is_ident(it.as_str()))?;
    let reason = reason.map_or(UnstableReason::None, UnstableReason::Some);
    Some(Stability {
        level: StabilityLevel::Unstable { reason, issue, is_soft, implied_by, old_name },
        feature,
    })
}

/// An attribute list's item: its key, and what follows it.
type Items<'t> = SmallVec<[(ast::Token<'t>, Value<'t>); 4]>;

enum Value<'t> {
    /// Nothing: `soft`.
    Word,
    /// `= "string"`.
    Str(ast::String<'t>),
    /// Anything else, which no key takes as its value.
    Other,
}

/// The items of an attribute's list, each starting with a name; `None` when one doesn't (a
/// literal, rustc's "unsupported literal") or one is empty but the last.
fn items(tt: ast::TokenTree<'_>) -> Option<Items<'_>> {
    let mut tokens: SmallVec<[ast::Element<'_>; 16]> =
        // alloc: a 16-element stack buffer; spills only for a longer attribute list
        tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).skip(1).collect();
    if tokens.last().is_some_and(|it| it.kind() == T![')']) {
        tokens.pop();
    }
    let mut items = Items::new();
    let mut split = tokens.split(|it| it.kind() == T![,]).peekable();
    while let Some(item) = split.next() {
        let [NodeOrToken::Token(key), rest @ ..] = item else {
            if item.is_empty() && split.peek().is_none() {
                break;
            }
            return None;
        };
        if key.kind() != SyntaxKind::IDENT {
            return None;
        }
        let value = match rest {
            [] => Value::Word,
            [NodeOrToken::Token(eq), NodeOrToken::Token(value)]
                if eq.kind() == T![=] && value.kind() == SyntaxKind::STRING =>
            {
                ast::String::cast(*value).map_or(Value::Other, Value::Str)
            }
            _ => Value::Other,
        };
        items.push((*key, value));
    }
    Some(items)
}

fn key_text(key: ast::Token<'_>) -> &str {
    let text = key.text();
    text.strip_prefix("r#").unwrap_or(text)
}

/// `insert_value_into_option_or_error`: a key given once, its value a string.
fn insert(slot: &mut Option<Symbol>, value: &Value<'_>) -> Option<()> {
    let (None, Value::Str(value)) = (&slot, value) else { return None };
    *slot = Some(Symbol::intern(&value.value().ok()?));
    Some(())
}

/// The string literal `expr` is, through parentheses.
fn string_literal(mut expr: ast::Expr<'_>) -> Option<Symbol> {
    while let ast::Expr::ParenExpr(paren) = &expr {
        expr = paren.expr()?;
    }
    let ast::Expr::Literal(literal) = expr else { return None };
    let ast::LiteralKind::String(string) = literal.kind() else { return None };
    Some(Symbol::intern(&string.value().ok()?))
}

/// rustc_lexer's `is_ident`, for a feature's name.
fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|it| it == '_' || unicode_ident::is_xid_start(it))
        && chars.all(unicode_ident::is_xid_continue)
}

/// The definition `def` is in, as rustc's def path has it (`opt_local_parent`): an item's
/// module, impl, trait or extern block, a variant's enum, a module's parent; none for a crate
/// root. A block's items are in the definition the block is written in.
fn parent(db: &Db, def: AttrDefId) -> Option<StabilityDef> {
    let container = match def {
        AttrDefId::ModuleId(module) => {
            let def_map = module.def_map(db);
            return match def_map.module(db, module).parent {
                Some(parent) => in_module(db, parent),
                None => Some(block_owner(db, def_map.block_id()?)),
            };
        }
        AttrDefId::EnumVariantId(it) => {
            return Some(AttrDefId::AdtId(AdtId::EnumId(it.lookup(db).parent)).into());
        }
        AttrDefId::FunctionId(it) => it.lookup(db).container,
        AttrDefId::ConstId(it) => it.lookup(db).container,
        AttrDefId::StaticId(it) => it.lookup(db).container,
        AttrDefId::TypeAliasId(it) => it.lookup(db).container,
        AttrDefId::AdtId(AdtId::StructId(it)) => it.lookup(db).container.into(),
        AttrDefId::AdtId(AdtId::UnionId(it)) => it.lookup(db).container.into(),
        AttrDefId::AdtId(AdtId::EnumId(it)) => it.lookup(db).container.into(),
        AttrDefId::TraitId(it) => it.lookup(db).container.into(),
        AttrDefId::ImplId(it) => it.lookup(db).container.into(),
        AttrDefId::ExternBlockId(it) => it.lookup(db).container.into(),
        AttrDefId::ExternCrateId(it) => it.lookup(db).container.into(),
        AttrDefId::UseId(it) => it.lookup(db).container.into(),
        AttrDefId::MacroId(it) => it.module(db).into(),
    };
    match container {
        ItemContainerId::ModuleId(module) => in_module(db, module),
        ItemContainerId::ImplId(it) => Some(AttrDefId::ImplId(it).into()),
        ItemContainerId::TraitId(it) => Some(AttrDefId::TraitId(it).into()),
        ItemContainerId::ExternBlockId(it) => Some(AttrDefId::ExternBlockId(it).into()),
    }
}

/// The definition an item declared in `module` is in: the module, or for a block's own
/// module, which rustc has no definition for, the definition the block is written in.
fn in_module(db: &Db, module: ModuleId) -> Option<StabilityDef> {
    let def_map = module.def_map(db);
    match def_map.block_id() {
        Some(block) if def_map.module(db, module).parent.is_none() => Some(block_owner(db, block)),
        _ => Some(AttrDefId::ModuleId(module).into()),
    }
}

/// The definition whose body or signature `block` is written in: the nearest of its
/// ancestors, through the macro calls it is expanded from, that is one. A block def map
/// exists only for a block a definition's syntax holds, so there is always one; not finding
/// it is a bug of the def maps, and stops.
#[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
fn block_owner<'db>(db: &'db Db, block: BlockIdLt<'db>) -> StabilityDef {
    let ast_id = block.ast_id(db);
    let no_owner = || -> ! { panic!("no definition of its module holds the block at {ast_id:?} with items") };
    let node = InFile::new(ast_id.file_id, ast_id.to_view(db).syntax())
        .parent_ancestors_with_macros_view(db)
        .find(|it| is_definition(it.value.kind()))
        .unwrap_or_else(|| no_owner());
    let mut module = block.module(db);
    loop {
        if let Some(found) = definition_at(db, module, node) {
            return found;
        }
        // The module of a block in a block of the same definition is the outer block's.
        match module.block(db) {
            Some(outer) if module.is_block_module(db) => module = outer.module(db),
            _ => no_owner(),
        }
    }
}

fn is_definition(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::FN
            | SyntaxKind::CONST
            | SyntaxKind::STATIC
            | SyntaxKind::STRUCT
            | SyntaxKind::ENUM
            | SyntaxKind::UNION
            | SyntaxKind::TRAIT
            | SyntaxKind::IMPL
            | SyntaxKind::TYPE_ALIAS
            | SyntaxKind::VARIANT
            | SyntaxKind::RECORD_FIELD
            | SyntaxKind::TUPLE_FIELD
    )
}

/// The definition declared in `module` (or in an impl or trait of it) whose syntax `node` is.
fn definition_at(
    db: &Db,
    module: ModuleIdLt<'_>,
    node: InFile<ast::Node<'_>>,
) -> Option<StabilityDef> {
    let file = node.file_id;
    match node.value.kind() {
        SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD => {
            let owner = node.value.ancestors().find(|it| {
                matches!(it.kind(), SyntaxKind::STRUCT | SyntaxKind::UNION | SyntaxKind::VARIANT)
            })?;
            let variant = match definition_at(db, module, node.with_value(owner))? {
                StabilityDef::Def(AttrDefId::AdtId(AdtId::StructId(it))) => VariantId::StructId(it),
                StabilityDef::Def(AttrDefId::AdtId(AdtId::UnionId(it))) => VariantId::UnionId(it),
                StabilityDef::Def(AttrDefId::EnumVariantId(it)) => VariantId::EnumVariantId(it),
                _ => return None,
            };
            let range = node.value.text_range();
            let mut found = None;
            for_each_field(db, variant, Tree::Live, |at, _, field| {
                if field.file_id == file && field.value.syntax().text_range() == range {
                    found = Some(at);
                }
            });
            let local_id = Idx::from_raw(RawIdx::from_u32(found?));
            Some(FieldId { parent: variant, local_id }.into())
        }
        SyntaxKind::VARIANT => {
            let owner = node.value.ancestors().find(|it| it.kind() == SyntaxKind::ENUM)?;
            let StabilityDef::Def(AttrDefId::AdtId(AdtId::EnumId(it))) =
                definition_at(db, module, node.with_value(owner))?
            else {
                return None;
            };
            let id = file
                .ast_id_map(db)
                .ast_id_of_view::<syntax::ast::Variant>(ast::Variant::cast(node.value)?)
                .erase();
            it.enum_variants(db)
                .variants
                .values()
                .find(|(variant, _)| same(variant.lookup(db).id, file, id))
                .map(|(variant, _)| AttrDefId::EnumVariantId(*variant).into())
        }
        _ => {
            let id = file
                .ast_id_map(db)
                .ast_id_of_view::<syntax::ast::Item>(ast::Item::cast(node.value)?)
                .erase();
            let scope = module.def_map(db).module(db, module).scope(db);
            let in_items = |items: &[(Name, AssocItemId)]| -> Option<StabilityDef> {
                items.iter().find_map(|&(_, item)| {
                    let found = match item {
                        AssocItemId::FunctionId(it) => same(it.lookup(db).id, file, id),
                        AssocItemId::ConstId(it) => same(it.lookup(db).id, file, id),
                        AssocItemId::TypeAliasId(it) => same(it.lookup(db).id, file, id),
                    };
                    found.then(|| AttrDefId::from(item).into())
                })
            };
            for def in scope.declarations() {
                let found = match def {
                    ModuleDefId::FunctionId(it) => same(it.lookup(db).id, file, id),
                    ModuleDefId::ConstId(it) => same(it.lookup(db).id, file, id),
                    ModuleDefId::StaticId(it) => same(it.lookup(db).id, file, id),
                    ModuleDefId::TypeAliasId(it) => same(it.lookup(db).id, file, id),
                    ModuleDefId::AdtId(AdtId::StructId(it)) => same(it.lookup(db).id, file, id),
                    ModuleDefId::AdtId(AdtId::UnionId(it)) => same(it.lookup(db).id, file, id),
                    ModuleDefId::AdtId(AdtId::EnumId(it)) => same(it.lookup(db).id, file, id),
                    ModuleDefId::TraitId(it) => {
                        if let Some(found) = in_items(&TraitItems::query(db, it).items[..]) {
                            return Some(found);
                        }
                        same(it.lookup(db).id, file, id)
                    }
                    _ => false,
                };
                if found {
                    let def: AttrDefId = match def {
                        ModuleDefId::FunctionId(it) => it.into(),
                        ModuleDefId::ConstId(it) => it.into(),
                        ModuleDefId::StaticId(it) => it.into(),
                        ModuleDefId::TypeAliasId(it) => it.into(),
                        ModuleDefId::AdtId(it) => it.into(),
                        ModuleDefId::TraitId(it) => it.into(),
                        _ => return None,
                    };
                    return Some(def.into());
                }
            }
            if let Some(it) = scope.unnamed_consts().find(|it| same(it.lookup(db).id, file, id)) {
                return Some(AttrDefId::ConstId(it).into());
            }
            for it in scope.impls() {
                if same(it.lookup(db).id, file, id) {
                    return Some(AttrDefId::ImplId(it).into());
                }
                if let Some(found) = in_items(&ImplItems::of(db, it).0.items[..]) {
                    return Some(found);
                }
            }
            None
        }
    }
}

fn same<N: AstIdNode>(id: AstId<N>, file: HirFileId, erased: ErasedFileAstId) -> bool {
    id.file_id == file && id.value.erase() == erased
}
