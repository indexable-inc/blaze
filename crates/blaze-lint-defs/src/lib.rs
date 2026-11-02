//! Every lint rustc knows, as its `LintStore` holds them: the builtin lints in the order rustc
//! registers them, their groups (those `register_builtins` adds, and `future_incompatible` and
//! the edition groups rustc builds from the lints), the renamed, removed and ignored names, the
//! internal `rustc::` lints of `-Z unstable-options`, rustc's check of a lint name, and the lint
//! options of the command line. rustc's `rustc_lint_defs`, with the tables of its lint store
//! and `rustc_session`'s `lint_opts` and `lint_cap`: no queries and no syntax, so base-db holds
//! a crate's [`LintOptions`] and the front end (`hir-lint-levels`) and blaze-diagnostics read
//! one copy of the lints. The suggestion of a similar name for an unknown one is
//! blaze-diagnostics' (`no_lint_suggestion`); [`Checked::NoLint`] says when to look for one.
//!
//! Follows rustc's `compiler/rustc_lint_defs/src/{lib,builtin}.rs` (`declare_lint!`,
//! `FutureIncompatibleInfo`), every `declare_lint!` of `compiler/rustc_lint/src/`,
//! `compiler/rustc_lint/src/lib.rs` (`register_builtins`, `register_internals`),
//! `compiler/rustc_lint/src/context.rs` (`LintStore::register_lints`, `check_lint_name`,
//! `check_tool_name_for_backwards_compat`), `compiler/rustc_session/src/config.rs`
//! (`get_cmd_lint_options`), of rust-lang/rust ad3d0bc141a02cf446e384136d250a1f6950fed5 (MIT
//! or Apache-2.0); the pinned nightly is `nightly-2026-10-27`.
//!
//! Every lint knows its [`LintId`], and every name is found by a perfect hash built as blaze
//! is: see `index`. Nothing is built at run time but the groups' lists of lints, once. The
//! tables are hand-ported `const` data (`lints`, `names`) against the pinned nightly's
//! `declare_lint!`s, `register_builtins` and `register_internals`; `tests` holds their counts
//! and hashes to rustc's.

#![forbid(unsafe_code)]

mod index;
mod lints;
mod names;
mod options;
mod set;

use std::sync::OnceLock;

use edition::Edition;
use serde::{Deserialize, Serialize};

pub use self::{
    options::{Args, LintOptions},
    set::LintSet,
};
use self::index::{
    ALL, BLAZE_END, BUILTIN_END, Entry, FIRST_GROUP, GROUP_COUNT, INTERNAL_END, INTERNAL_GROUP,
    Joined, KNOWN_TOOLS, LINTS_BY_NAME, NO_GROUP, OTHER_KEYS, REFS, other,
};

/// A lint level, in rustc's order: where one is capped, the lower wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    Allow,
    Expect,
    Warn,
    ForceWarn,
    Deny,
    Forbid,
}

impl Level {
    /// As an attribute and rustc's notes name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Allow => "allow",
            Level::Expect => "expect",
            Level::Warn => "warn",
            Level::ForceWarn => "force-warn",
            Level::Deny => "deny",
            Level::Forbid => "forbid",
        }
    }

    /// The level an attribute named `name` sets: rustc's `Level::from_symbol`, by one perfect
    /// hash.
    pub fn of_attr(name: &str) -> Option<Level> {
        LEVEL_ATTRS.get(name).map(|at| ATTR_LEVELS[at])
    }

    /// The level `--cap-lints` names: rustc's `Level::from_str`, which has no `expect` nor
    /// `force-warn`.
    pub fn of_cap(name: &str) -> Option<Level> {
        Some(match name {
            "allow" => Level::Allow,
            "warn" => Level::Warn,
            "deny" => Level::Deny,
            "forbid" => Level::Forbid,
            _ => return None,
        })
    }

    /// The flag that sets it. `expect` has none; rustc never asks for it.
    pub fn to_cmd_flag(self) -> &'static str {
        match self {
            Level::Allow => "-A",
            Level::Expect | Level::Warn => "-W",
            Level::ForceWarn => "--force-warn",
            Level::Deny => "-D",
            Level::Forbid => "-F",
        }
    }

    pub fn is_error(self) -> bool {
        matches!(self, Level::Deny | Level::Forbid)
    }
}

/// The attributes that set a level, at the indices of their levels in [`ATTR_LEVELS`].
const LEVEL_ATTR_NAMES: &[&str] = &["allow", "expect", "warn", "deny", "forbid"];
const ATTR_LEVELS: [Level; 5] = [Level::Allow, Level::Expect, Level::Warn, Level::Deny, Level::Forbid];

blaze_phf::phf_table!(static LEVEL_ATTRS = LEVEL_ATTR_NAMES;);

/// Why a lint is future-incompatible: rustc's `FutureIncompatibilityReason`, with its issue or
/// the edition guide's page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// An error in a future release, in every edition.
    FutureReleaseError(u32),
    /// A change of meaning in a future release.
    FutureReleaseSemanticsChange(u32),
    /// As `FutureReleaseError`, said in the lint's own words.
    Custom(&'static str, u32),
    /// An error from the edition on.
    EditionError(Edition, &'static str),
    /// A change of meaning from the edition on.
    EditionSemanticsChange(Edition, &'static str),
    EditionAndFutureReleaseError(Edition, &'static str),
    EditionAndFutureReleaseSemanticsChange(Edition, &'static str),
}

impl Reason {
    /// The edition it is tied to, if any: such a lint is of that edition's compatibility group,
    /// any other of `future_incompatible`.
    pub const fn edition(self) -> Option<Edition> {
        match self {
            Reason::EditionError(edition, _)
            | Reason::EditionSemanticsChange(edition, _)
            | Reason::EditionAndFutureReleaseError(edition, _)
            | Reason::EditionAndFutureReleaseSemanticsChange(edition, _) => Some(edition),
            Reason::FutureReleaseError(_)
            | Reason::FutureReleaseSemanticsChange(_)
            | Reason::Custom(..) => None,
        }
    }

    /// Where to read more: rustc's `FutureIncompatibilityReason::reference`.
    pub fn reference(self) -> String {
        match self {
            Reason::FutureReleaseError(issue)
            | Reason::FutureReleaseSemanticsChange(issue)
            | Reason::Custom(_, issue) => {
                // alloc: a note's text, as a diagnostic is built
                format!("issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}>")
            }
            Reason::EditionError(edition, page)
            | Reason::EditionSemanticsChange(edition, page)
            | Reason::EditionAndFutureReleaseError(edition, page)
            | Reason::EditionAndFutureReleaseSemanticsChange(edition, page) => {
                // alloc: as above
                format!(
                    "<https://doc.rust-lang.org/edition-guide/rust-{}/{page}.html>",
                    edition.number()
                )
            }
        }
    }

    /// What rustc warns of it, in a crate of edition `current`.
    pub fn explanation(self, current: Edition) -> String {
        match self {
            // alloc: a note's text, as a diagnostic is built
            Reason::FutureReleaseError(_) => "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!".to_owned(),
            Reason::FutureReleaseSemanticsChange(_) => {
                // alloc: as above
                "this will change its meaning in a future release!".to_owned()
            }
            // alloc: as above
            Reason::EditionError(edition, _) => format!(
                "this is accepted in the current edition (Rust {}) but is a hard error in Rust {}!",
                current.number(),
                edition.number()
            ),
            Reason::EditionSemanticsChange(edition, _) => {
                // alloc: as above
                format!("this changes meaning in Rust {}", edition.number())
            }
            // alloc: as above
            Reason::EditionAndFutureReleaseError(edition, _) => format!(
                "this was previously accepted by the compiler but is being phased out; it will become a hard error in Rust {} and in a future release in all editions!",
                edition.number()
            ),
            // alloc: as above
            Reason::EditionAndFutureReleaseSemanticsChange(edition, _) => format!(
                "this changes meaning in Rust {} and in a future release in all editions!",
                edition.number()
            ),
            // alloc: as above
            Reason::Custom(text, _) => text.to_owned(),
        }
    }
}

/// rustc's `FutureIncompatibleInfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FutureIncompatible {
    pub reason: Reason,
    /// Whether rustc warns of the reason; off for a lint whose message says it already.
    pub explain_reason: bool,
    /// Whether it shows in cargo's future-incompatibility report of dependencies.
    pub report_in_deps: bool,
}

/// The feature a lint is unknown without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature {
    pub name: &'static str,
    /// Its tracking issue, as `rustc_feature` has it.
    pub issue: Option<u32>,
}

/// A lint: rustc's `Lint`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lint {
    /// Lowercase, as attributes name it; a tool's with its tool: `rustc::bad_opt_access`.
    pub name: &'static str,
    pub default_level: Level,
    pub desc: &'static str,
    /// From this edition on, this level is its default.
    pub edition_level: Option<(Edition, Level)>,
    /// Whether it is given in the expansion of another crate's macro.
    pub report_in_external_macro: bool,
    pub future_incompatible: Option<FutureIncompatible>,
    pub feature_gate: Option<Feature>,
    /// Whether only the crate's own attributes set its level.
    pub crate_level_only: bool,
    /// Whether rustc runs it when it is allowed.
    pub eval_always: bool,
    /// Whether `-D warnings` leaves it a warning.
    pub ignore_deny_warnings: bool,
    /// Whether a tool other than the compile gives it (rustc's, for clippy's lints): blaze-lints'
    /// under `blaze check`, which also says which of its `#[expect]`s are unfulfilled.
    pub is_externally_loaded: bool,
    /// Its index in the tables, set as they are numbered (`index::ALL`).
    id: LintId,
}

impl Lint {
    /// Its id: one load, no lookup.
    #[inline]
    pub const fn id(&self) -> LintId {
        self.id
    }

    /// Its default level in a crate of edition `edition`: rustc's `Lint::default_level`.
    pub fn default_level(&self, edition: Edition) -> Level {
        match self.edition_level {
            Some((since, level)) if since <= edition => level,
            _ => self.default_level,
        }
    }

    const fn new(name: &'static str, default_level: Level, desc: &'static str) -> Lint {
        Lint {
            name,
            default_level,
            desc,
            edition_level: None,
            report_in_external_macro: false,
            future_incompatible: None,
            feature_gate: None,
            crate_level_only: false,
            eval_always: false,
            ignore_deny_warnings: false,
            is_externally_loaded: false,
            id: LintId::UNNUMBERED,
        }
    }

    const fn external(mut self) -> Lint {
        self.report_in_external_macro = true;
        self
    }

    const fn crate_level(mut self) -> Lint {
        self.crate_level_only = true;
        self
    }

    const fn eval_always(mut self) -> Lint {
        self.eval_always = true;
        self
    }

    const fn ignore_deny_warnings(mut self) -> Lint {
        self.ignore_deny_warnings = true;
        self
    }

    const fn externally_loaded(mut self) -> Lint {
        self.is_externally_loaded = true;
        self
    }

    const fn edition(mut self, edition: Edition, level: Level) -> Lint {
        self.edition_level = Some((edition, level));
        self
    }

    const fn gate(mut self, name: &'static str, issue: Option<u32>) -> Lint {
        self.feature_gate = Some(Feature { name, issue });
        self
    }

    const fn fcw(mut self, reason: Reason) -> Lint {
        self.future_incompatible =
            Some(FutureIncompatible { reason, explain_reason: true, report_in_deps: false });
        self
    }

    /// `fcw` with `report_in_deps: true`.
    const fn fcw_deps(mut self, reason: Reason) -> Lint {
        self.future_incompatible =
            Some(FutureIncompatible { reason, explain_reason: true, report_in_deps: true });
        self
    }

    const fn unexplained(mut self) -> Lint {
        if let Some(it) = &mut self.future_incompatible {
            it.explain_reason = false;
        }
        self
    }
}

/// A lint group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: &'static str,
    /// In rustc's order: the order of `add_lint_group!`, or of registration.
    pub lints: Vec<&'static Lint>,
    /// For an alias rustc keeps without saying so (`bad_style`): the group it stands for.
    pub alias_of: Option<&'static str>,
    /// [`Group::lints`] as a set.
    members: LintSet,
}

impl Group {
    fn new(name: &'static str, lints: Vec<&'static Lint>, alias_of: Option<&'static str>) -> Group {
        // alloc: none, a `LintSet` is inline; and setup, once per process
        let members = lints.iter().map(|it| it.id).collect();
        Group { name, lints, alias_of, members }
    }

    /// Whether the lint `id` is of it: one bit test.
    #[inline]
    pub fn has(&self, id: LintId) -> bool {
        self.members.contains(id)
    }

    /// Its lints as a set.
    pub fn members(&self) -> &LintSet {
        &self.members
    }
}

/// What a name stands for in rustc's lint store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Lint(&'static Lint),
    Group(&'static Group),
    /// Renamed: the new name.
    Renamed(&'static str),
    /// Removed: why.
    Removed(&'static str),
    /// A rustdoc lint of old, which rustc takes and does nothing with.
    Ignored,
    Unknown,
}

/// The tools every crate has registered for lints and attributes (rustc_resolve's
/// `PREDEFINED_TOOLS`); `rustc` and `rustdoc` are tools of lints too, though not of attributes.
pub const PREDEFINED_TOOLS: &[&str] = &["clippy", "rustfmt", "diagnostic", "miri", "rust_analyzer"];

/// rustc's builtin lints, in the order it registers them.
pub fn lints() -> &'static [Lint] {
    &ALL[..BUILTIN_END]
}

/// The internal lints rustc registers under `-Z unstable-options`: `rustc::`.
pub fn internal_lints() -> &'static [Lint] {
    &ALL[BLAZE_END..INTERNAL_END]
}

/// The tool of blaze's own lints, which every crate has, as rustc has `rustc` and `rustdoc`.
pub const BLAZE_TOOL: &str = "blaze";

/// blaze's own lints, `blaze::`, registered in every crate after rustc's builtin ones.
pub fn blaze_lints() -> &'static [Lint] {
    &ALL[BUILTIN_END..BLAZE_END]
}

/// A lint as a `u16`: its index in the builtin lints, then blaze's, the internal ones and the
/// unregistered ones. Comparing or hashing one is comparing two bytes, not a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LintId(u16);

const _: () = assert!(size_of::<LintId>() == 2);

impl LintId {
    /// The id of a lint before the tables are numbered; no lint of them keeps it.
    const UNNUMBERED: LintId = LintId(u16::MAX);

    pub const WARNINGS: LintId = LintId::named("warnings");
    pub const UNFULFILLED_LINT_EXPECTATIONS: LintId = LintId::named("unfulfilled_lint_expectations");
    pub const RENAMED_AND_REMOVED_LINTS: LintId = LintId::named("renamed_and_removed_lints");
    pub const UNKNOWN_LINTS: LintId = LintId::named("unknown_lints");
    pub const UNUSED_ATTRIBUTES: LintId = LintId::named("unused_attributes");
    pub const FORBIDDEN_LINT_GROUPS: LintId = LintId::named("forbidden_lint_groups");
    pub const DEAD_CODE: LintId = LintId::named("dead_code");
    pub const SINGLE_USE_LIFETIMES: LintId = LintId::named("single_use_lifetimes");
    pub const MISSING_DOCS: LintId = LintId::named("missing_docs");
    pub const LONG_RUNNING_CONST_EVAL: LintId = LintId::named("long_running_const_eval");

    /// The builtin lint named `name`, found at compile time when it is a `const`: a name that
    /// is no builtin lint's fails to compile.
    pub const fn named(name: &str) -> LintId {
        match index::scan(name, BUILTIN_END) {
            Some(at) => LintId(at as u16),
            None => panic!("a named `LintId` names no builtin lint"),
        }
    }

    /// The id of `lint`: [`Lint::id`].
    #[inline]
    pub const fn of(lint: &Lint) -> LintId {
        lint.id
    }

    /// The lint of this id: one load.
    #[inline]
    pub fn lint(self) -> &'static Lint {
        &ALL[usize::from(self.0)]
    }

    /// Its index in the tables: `0..` the number of lints.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// `&'static [&'static Lint]` of it alone.
    fn alone(self) -> &'static [&'static Lint] {
        std::slice::from_ref(&REFS[usize::from(self.0)])
    }
}

/// Every named [`LintId`] is evaluated, and so checked against the table, as this crate
/// compiles.
const _: [LintId; 10] = [
    LintId::WARNINGS,
    LintId::UNFULFILLED_LINT_EXPECTATIONS,
    LintId::RENAMED_AND_REMOVED_LINTS,
    LintId::UNKNOWN_LINTS,
    LintId::UNUSED_ATTRIBUTES,
    LintId::FORBIDDEN_LINT_GROUPS,
    LintId::DEAD_CODE,
    LintId::SINGLE_USE_LIFETIMES,
    LintId::MISSING_DOCS,
    LintId::LONG_RUNNING_CONST_EVAL,
];

/// The builtin lint, or blaze's, named `name`, lowercase, as the store has it: one perfect
/// hash of the name.
pub fn lint(name: &str) -> Option<&'static Lint> {
    let at = LINTS_BY_NAME.get(name)?;
    (at < BLAZE_END).then(|| &ALL[at])
}

/// The lints rustc declares and does not register; their names are unknown to attributes.
pub fn unregistered() -> &'static [Lint] {
    &ALL[INTERNAL_END..]
}

/// Every lint of the tables, by [`LintId`]: the builtin ones, blaze's, the internal ones and
/// the unregistered ones.
pub fn all() -> &'static [Lint] {
    &ALL
}

/// The lint a diagnostic named `name` is of: a builtin one, an internal one, or one rustc
/// emits without registering it (`private_macro_use`).
pub fn declared(name: &str) -> Option<&'static Lint> {
    LINTS_BY_NAME.get(name).map(|at| &ALL[at])
}

/// The builtin lint group named `name`, aliases included.
pub fn group(name: &str) -> Option<&'static Group> {
    match other(name.as_bytes()) {
        Some(Entry::Group(at)) => Some(&groups()[usize::from(at)]),
        _ => None,
    }
}

/// rustc's lint groups, in the order it adds them: those it makes of the future-incompatible
/// lints as it registers them, those of `register_builtins`, then the alias `bad_style`; after
/// them blaze's, `blaze::hot`.
pub fn groups() -> &'static [Group] {
    static GROUPS: OnceLock<Vec<Group>> = OnceLock::new();
    GROUPS.get_or_init(build_groups)
}

/// The internal group, `rustc::internal`.
pub fn internal_group() -> &'static Group {
    static GROUP: OnceLock<Group> = OnceLock::new();
    GROUP.get_or_init(|| {
        let lints = names::INTERNAL_GROUP
            .iter()
            .map(|name| match LINTS_BY_NAME.get(name) {
                Some(at) if (BLAZE_END..INTERNAL_END).contains(&at) => &ALL[at],
                _ => panic!("no internal lint `{name}`"),
            })
            // alloc: setup, once per process
            .collect();
        Group::new(INTERNAL_GROUP, lints, None)
    })
}

/// What `name` stands for among rustc's builtin lints and groups.
pub fn named(name: &str) -> Named {
    if let Some(lint) = lint(name) {
        return Named::Lint(lint);
    }
    match other(name.as_bytes()) {
        Some(Entry::Group(at)) => Named::Group(&groups()[usize::from(at)]),
        Some(Entry::Renamed(new)) => Named::Renamed(new),
        Some(Entry::Removed(reason)) => Named::Removed(reason),
        Some(Entry::Ignored) => Named::Ignored,
        Some(Entry::InternalGroup) | None => Named::Unknown,
    }
}

/// The renamed lints: the old name and the new.
pub fn renamed() -> &'static [(&'static str, &'static str)] {
    names::RENAMED
}

/// The removed lints and why.
pub fn removed() -> &'static [(&'static str, &'static str)] {
    names::REMOVED
}

/// The lint `name` is renamed to, following each renaming.
pub fn renamed_to(name: &str) -> Option<&'static str> {
    match other(name.as_bytes()) {
        Some(Entry::Renamed(new)) => Some(new),
        _ => None,
    }
}

/// The groups, at the indices the perfect hash of names gives them (`index::OTHER_KEYS`).
fn build_groups() -> Vec<Group> {
    let builtin = lints();
    let mut groups: Vec<Group> = Vec::with_capacity(GROUP_COUNT);
    for (name, kind) in index::fcw_groups() {
        // alloc: setup, once per process (`GROUPS`)
        let lints = builtin.iter().filter(|it| index::fcw_group(it) == Some(kind)).collect();
        groups.push(Group::new(name, lints, None));
    }
    for &(name, members) in names::GROUPS {
        let lints = members
            .iter()
            .map(|member| match LINTS_BY_NAME.get(member) {
                Some(at) if at < BUILTIN_END => &ALL[at],
                _ => panic!("group `{name}` names no lint `{member}`"),
            })
            // alloc: setup, as above
            .collect();
        groups.push(Group::new(name, lints, None));
    }
    for &(alias, of) in names::ALIASES {
        let lints = match other(of.as_bytes()) {
            // clone: setup, as above
            Some(Entry::Group(at)) if usize::from(at) < groups.len() => groups[usize::from(at)].lints.clone(),
            _ => panic!("alias `{alias}` of no group `{of}`"),
        };
        groups.push(Group::new(alias, lints, Some(of)));
    }
    for &(name, members) in names::BLAZE_GROUPS {
        let lints = members
            .iter()
            .map(|member| match LINTS_BY_NAME.get(member) {
                Some(at) if (BUILTIN_END..BLAZE_END).contains(&at) => &ALL[at],
                _ => panic!("group `{name}` names no lint of blaze's `{member}`"),
            })
            // alloc: setup, once per process (`GROUPS`)
            .collect();
        groups.push(Group::new(name, lints, None));
    }
    for (at, group) in groups.iter().enumerate() {
        assert_eq!(OTHER_KEYS[at], group.name, "the groups are where the perfect hash has them");
    }
    groups
}

/// The group rustc puts the lints of an edition in: `Edition::lint_name`.
pub const fn edition_group(edition: Edition) -> &'static str {
    match edition {
        Edition::Edition2015 => "rust_2015_compatibility",
        Edition::Edition2018 => "rust_2018_compatibility",
        Edition::Edition2021 => "rust_2021_compatibility",
        Edition::Edition2024 => "rust_2024_compatibility",
    }
}

/// A name of the store that is renamed, removed or ignored: what rustc's `by_name` has of it
/// that is neither a lint nor a group.
fn renaming(name: &[u8]) -> Option<Entry> {
    other(name).filter(|it| matches!(it, Entry::Renamed(_) | Entry::Removed(_) | Entry::Ignored))
}

/// What rustc's `check_lint_name` finds of a name: `CheckLintNameResult`. The lists are the
/// tables' own, with nothing allocated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checked {
    /// The lints it stands for: one, a group's, or none for an ignored name.
    Ok(&'static [&'static Lint]),
    /// Unknown. rustc suggests a similar name when `suggest`: of `tool::name` when a tool was
    /// written, else of `name` among clippy's (blaze-diagnostics' `no_lint_suggestion`).
    NoLint { suggest: bool },
    /// Of a tool rustc does not know.
    NoTool,
    Renamed(&'static str),
    Removed(&'static str),
    /// Of a tool, with the name to use instead when this one is deprecated.
    Tool(&'static [&'static Lint], Option<String>),
    /// Of a tool that is not loaded: rustc says nothing of it.
    MissingTool,
}

/// rustc's lint store as one crate is checked with it: the builtin lints, the internal ones
/// under `-Z unstable-options`, and the tools of `#![register_tool]` and
/// `#![register_lint_tool]` (those of [`PREDEFINED_TOOLS`] always).
#[derive(Clone, Debug, Default)]
pub struct Store {
    pub internal: bool,
    pub registered_tools: Vec<String>,
}

impl Store {
    /// The lints registered, in rustc's order.
    pub fn lints(&self) -> impl Iterator<Item = &'static Lint> {
        let end = if self.internal { INTERNAL_END } else { BLAZE_END };
        ALL[..end].iter()
    }

    /// The groups, in rustc's order, aliases included.
    pub fn groups(&self) -> impl Iterator<Item = &'static Group> {
        let internal = self.internal.then(internal_group);
        groups().iter().chain(internal)
    }

    /// Whether some lint registered is `tool`'s.
    fn has_lints_of(&self, tool: &str) -> bool {
        index::has_lints_of(tool, self.internal)
    }

    /// The lint registered of the name `name`: one perfect hash.
    fn by_name(&self, name: &[u8]) -> Option<&'static Lint> {
        let at = LINTS_BY_NAME.index(name)?;
        (at < BLAZE_END || (self.internal && at < INTERNAL_END)).then(|| &ALL[at])
    }

    fn group(&self, name: &[u8]) -> Option<&'static Group> {
        match other(name)? {
            Entry::Group(at) => Some(&groups()[usize::from(at)]),
            Entry::InternalGroup if self.internal => Some(internal_group()),
            _ => None,
        }
    }

    /// Whether `name` is a group's, or `warnings`: rustc's `is_lint_group`.
    pub fn is_lint_group(&self, name: &str) -> bool {
        name == "warnings" || self.group(name.as_bytes()).is_some()
    }

    /// The lints `name` stands for: rustc's `find_lints`.
    pub fn find_lints(&self, name: &str) -> Option<&'static [&'static Lint]> {
        if let Some(lint) = self.by_name(name.as_bytes()) {
            return Some(lint.id.alone());
        }
        match other(name.as_bytes())? {
            Entry::Renamed(new) => Some(self.by_name(new.as_bytes())?.id.alone()),
            Entry::Removed(_) => None,
            Entry::Ignored => Some(&[][..]),
            Entry::Group(at) => Some(groups()[usize::from(at)].lints.as_slice()),
            Entry::InternalGroup => self.internal.then(|| &internal_group().lints[..]),
        }
    }

    /// The lints a lint attribute's or a lint flag's name `name`, of `tool` when one is
    /// written, sets the level of: those [`Store::check`] gives it, the new name's for a
    /// renamed one, none for an unknown one. Two perfect hashes at most, and nothing allocated
    /// or suggested; a tool this store does not know of is not told apart from one it does,
    /// as no lint of the tables is an unknown tool's.
    pub fn lints_named(&self, tool: Option<&str>, name: &str) -> &'static [&'static Lint] {
        let joined = tool.map(|tool| Joined::new(tool, name));
        let complete = match &joined {
            Some(joined) => joined.bytes(),
            None => name.as_bytes(),
        };
        if let Some(lint) = self.by_name(complete) {
            return lint.id.alone();
        }
        match other(complete) {
            Some(Entry::Renamed(new)) => self.find_lints(new).unwrap_or(&[]),
            Some(Entry::Group(at)) => &groups()[usize::from(at)].lints[..],
            Some(Entry::InternalGroup) if self.internal => &internal_group().lints[..],
            Some(_) => &[],
            None if tool.is_some() => &[],
            None => {
                let clippy = Joined::new("clippy", name);
                match self.by_name(clippy.bytes()) {
                    Some(lint) => lint.id.alone(),
                    None => self.group(clippy.bytes()).map_or(&[][..], |it| &it.lints[..]),
                }
            }
        }
    }

    /// rustc's `check_lint_name` of `name`, written with `tool::` before it when `tool` is
    /// given. Nothing is allocated unless the name is deprecated.
    pub fn check(&self, name: &str, tool: Option<&str>) -> Checked {
        if let Some(tool) = tool
            && KNOWN_TOOLS.get(tool).is_none()
            && !self.registered_tools.iter().any(|it| it == tool)
        {
            return Checked::NoTool;
        }
        let joined = tool.map(|tool| Joined::new(tool, name));
        let complete = match &joined {
            Some(joined) => joined.bytes(),
            None => name.as_bytes(),
        };
        if let Some(tool) = tool {
            if let Some(lint) = self.by_name(complete) {
                return Checked::Tool(lint.id.alone(), None);
            }
            if renaming(complete).is_none() {
                return match self.group(complete) {
                    // Every alias of rustc's is a quiet one: none is said to be deprecated.
                    Some(group) => Checked::Tool(&group.lints, None),
                    None => {
                        if self.has_lints_of(tool) {
                            Checked::NoLint { suggest: true }
                        } else {
                            Checked::MissingTool
                        }
                    }
                };
            }
        }
        if let Some(lint) = self.by_name(complete) {
            return Checked::Ok(lint.id.alone());
        }
        match renaming(complete) {
            Some(Entry::Renamed(new)) => Checked::Renamed(new),
            Some(Entry::Removed(reason)) => Checked::Removed(reason),
            Some(_) => Checked::Ok(&[]),
            // Here `tool` is `None`: a tool's name that is not renamed, removed nor ignored
            // returned above.
            None => match self.group(complete) {
                Some(group) => Checked::Ok(&group.lints),
                None => self.backwards_compat(name, "clippy"),
            },
        }
    }

    /// rustc's `check_tool_name_for_backwards_compat`: a name of clippy's written without
    /// `clippy::`, which rustc knows when clippy is loaded and this store never is.
    fn backwards_compat(&self, name: &str, tool: &str) -> Checked {
        let joined = Joined::new(tool, name);
        let complete = joined.bytes();
        if let Some(lint) = self.by_name(complete) {
            // alloc: the name a warning suggests, for a clippy lint written without `clippy::`
            return Checked::Tool(lint.id.alone(), Some(format!("{tool}::{name}")));
        }
        if renaming(complete).is_some() {
            return Checked::NoLint { suggest: false };
        }
        match self.group(complete) {
            // alloc: as above
            Some(group) => Checked::Tool(&group.lints, Some(format!("{tool}::{name}"))),
            None => Checked::NoLint { suggest: true },
        }
    }

    /// The first group, aliases left out, that has `lint`: what rustc's note of a default
    /// level names it part of.
    pub fn group_of(&self, lint: &'static Lint) -> Option<&'static str> {
        match FIRST_GROUP.get(usize::from(lint.id.0)) {
            Some(&first) => (first != NO_GROUP).then(|| OTHER_KEYS[usize::from(first)]),
            None => (self.internal && internal_group().has(lint.id)).then_some(INTERNAL_GROUP),
        }
    }
}

#[cfg(test)]
mod tests;
