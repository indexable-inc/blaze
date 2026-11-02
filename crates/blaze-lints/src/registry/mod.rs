//! What clippy declares of each lint: its name, its group and so its default level, and its
//! description; the groups by name, as `#[warn(clippy::pedantic)]` names them; and the lints it
//! renamed and removed. The table is clippy's own, written from its source
//! (`harness/clippy_registry.py`).

mod clippy;

use serde::{Deserialize, Serialize};

use crate::Level;

/// A lint of clippy's.
#[derive(Debug, PartialEq, Eq)]
pub struct Info {
    /// Lowercase, without `clippy::`.
    pub name: &'static str,
    pub group: Group,
    pub desc: &'static str,
    /// The release it came in, as clippy's `#[clippy::version]` says.
    pub version: &'static str,
}

/// clippy's lint groups (`LintCategory`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Group {
    Cargo,
    Complexity,
    Correctness,
    Nursery,
    Pedantic,
    Perf,
    Restriction,
    Style,
    Suspicious,
}

impl Group {
    pub const ALL: [Group; 9] = [
        Group::Cargo,
        Group::Complexity,
        Group::Correctness,
        Group::Nursery,
        Group::Pedantic,
        Group::Perf,
        Group::Restriction,
        Group::Style,
        Group::Suspicious,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Group::Cargo => "cargo",
            Group::Complexity => "complexity",
            Group::Correctness => "correctness",
            Group::Nursery => "nursery",
            Group::Pedantic => "pedantic",
            Group::Perf => "perf",
            Group::Restriction => "restriction",
            Group::Style => "style",
            Group::Suspicious => "suspicious",
        }
    }

    /// The level of its lints when nothing sets one: `declare_clippy_lint!`'s.
    pub fn default_level(self) -> Level {
        match self {
            Group::Correctness => Level::Deny,
            Group::Complexity | Group::Perf | Group::Style | Group::Suspicious => Level::Warn,
            Group::Cargo | Group::Nursery | Group::Pedantic | Group::Restriction => Level::Allow,
        }
    }

    /// Whether `clippy::all` (`clippy::default`) has its lints.
    pub fn in_all(self) -> bool {
        self.default_level() != Level::Allow
    }
}

impl Info {
    pub fn default_level(&self) -> Level {
        self.group.default_level()
    }

    /// Its index in [`lints`]. Every `&Info` is one of the table's.
    #[inline]
    pub fn index(&self) -> LintIndex {
        let at = clippy::LINTS.element_offset(self).expect("an `Info` of the registry's table");
        LintIndex(at as u16)
    }

    /// The page of clippy's documentation for it, as the pinned nightly's clippy links it
    /// (`clippy_utils::diagnostics::docs_link`, a nightly's channel being `main`).
    pub fn docs_url(&self) -> String {
        format!("https://rust-lang.github.io/rust-clippy/main/index.html#{}", self.name)
    }
}

/// Every lint of clippy's, by name.
pub fn lints() -> &'static [Info] {
    clippy::LINTS
}

/// A lint of clippy's as its index in [`lints`]: comparing or hashing one is two bytes, not a
/// name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LintIndex(u16);

impl LintIndex {
    #[inline]
    pub fn info(self) -> &'static Info {
        &clippy::LINTS[usize::from(self.0)]
    }

    #[inline]
    pub(crate) const fn get(self) -> usize {
        self.0 as usize
    }
}

/// How many lints [`lints`] has.
pub(crate) const LINT_COUNT: usize = clippy::LINTS.len();

const _: () = assert!(LINT_COUNT < u16::MAX as usize);

const LINT_NAMES: [&str; LINT_COUNT] = {
    let mut names = [""; LINT_COUNT];
    let mut i = 0;
    while i < LINT_COUNT {
        names[i] = clippy::LINTS[i].name;
        i += 1;
    }
    names
};

blaze_phf::phf_table!(
    /// Each lint's index in [`lints`] from its name.
    static BY_NAME = &LINT_NAMES;
);

/// The names after `clippy::` that are not a lint's: `all` and `default`, the groups, the
/// renamed lints and the removed ones, each with what it stands for.
const OTHER_COUNT: usize = 2 + Group::ALL.len() + clippy::RENAMED.len() + clippy::REMOVED.len();

const OTHERS: [(&str, Named); OTHER_COUNT] = {
    let mut others = [("", Named::All); OTHER_COUNT];
    others[0] = ("all", Named::All);
    others[1] = ("default", Named::All);
    let mut at = 2;
    let mut i = 0;
    while i < Group::ALL.len() {
        others[at] = (Group::ALL[i].name(), Named::Group(Group::ALL[i]));
        at += 1;
        i += 1;
    }
    i = 0;
    while i < clippy::RENAMED.len() {
        others[at] = (clippy::RENAMED[i].0, Named::Renamed(clippy::RENAMED[i].1));
        at += 1;
        i += 1;
    }
    i = 0;
    while i < clippy::REMOVED.len() {
        others[at] = (clippy::REMOVED[i].0, Named::Removed(clippy::REMOVED[i].1));
        at += 1;
        i += 1;
    }
    others
};

const OTHER_NAMES: [&str; OTHER_COUNT] = {
    let mut names = [""; OTHER_COUNT];
    let mut i = 0;
    while i < OTHER_COUNT {
        names[i] = OTHERS[i].0;
        i += 1;
    }
    names
};

blaze_phf::phf_table!(static OTHER_BY_NAME = &OTHER_NAMES;);

/// The lint named `name`, without `clippy::`: one perfect hash of the name.
#[inline]
pub fn lint(name: &str) -> Option<&'static Info> {
    BY_NAME.get(name).map(|at| &clippy::LINTS[at])
}

/// The index of the lint named `name` in a `const`, for tables built from the lints' names.
pub(crate) const fn lint_index_const(name: &str) -> Option<LintIndex> {
    match BY_NAME.index_const(name.as_bytes()) {
        Some(at) => Some(LintIndex(at as u16)),
        None => None,
    }
}

/// What a name after `clippy::` in a lint attribute or flag stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Lint(&'static Info),
    Group(Group),
    /// `clippy::all`, or `clippy::default`: every lint of the groups that are not allowed.
    All,
    /// A lint clippy renamed; to its name now, which may be rustc's.
    Renamed(&'static str),
    /// A lint clippy removed, and why.
    Removed(&'static str),
}

/// What `name`, after `clippy::`, stands for, if clippy knows it.
pub fn named(name: &str) -> Option<Named> {
    if let Some(info) = lint(name) {
        return Some(Named::Lint(info));
    }
    OTHER_BY_NAME.get(name).map(|at| OTHERS[at].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_clippys() {
        let names: Vec<_> = lints().iter().map(|it| it.name).collect();
        assert!(names.is_sorted(), "clippy's table is by name");
        assert!(names.len() > 800);
        let correctness = lints().iter().filter(|it| it.group == Group::Correctness).count();
        assert_eq!(correctness, 67);
        assert_eq!(lint("needless_bool").map(|it| it.group), Some(Group::Complexity));
        assert_eq!(lint("eq_op").map(Info::default_level), Some(Level::Deny));
        assert_eq!(
            lint("collapsible_if").unwrap().docs_url(),
            "https://rust-lang.github.io/rust-clippy/main/index.html#collapsible_if"
        );
        assert_eq!(named("pedantic"), Some(Named::Group(Group::Pedantic)));
        assert_eq!(named("all"), Some(Named::All));
        assert_eq!(named("blacklisted_name"), Some(Named::Renamed("clippy::disallowed_names")));
        assert!(matches!(named("unused_collect"), Some(Named::Removed(_))));
        assert_eq!(named("no_such_lint"), None);
    }

    /// `named` as the scans the perfect hashes replace had it.
    fn named_by_scan(name: &str) -> Option<Named> {
        if let Some(info) = lints().iter().find(|it| it.name == name) {
            return Some(Named::Lint(info));
        }
        if name == "all" || name == "default" {
            return Some(Named::All);
        }
        if let Some(group) = Group::ALL.into_iter().find(|it| it.name() == name) {
            return Some(Named::Group(group));
        }
        if let Some((_, new)) = clippy::RENAMED.iter().find(|(old, _)| *old == name) {
            return Some(Named::Renamed(new));
        }
        if let Some((_, why)) = clippy::REMOVED.iter().find(|(old, _)| *old == name) {
            return Some(Named::Removed(why));
        }
        None
    }

    #[test]
    fn every_name_is_found_as_by_a_scan() {
        let mut names: Vec<String> = Vec::new();
        let known = lints()
            .iter()
            .map(|it| it.name)
            .chain(["all", "default"])
            .chain(Group::ALL.map(Group::name))
            .chain(clippy::RENAMED.iter().map(|it| it.0))
            .chain(clippy::REMOVED.iter().map(|it| it.0));
        for name in known {
            names.push(name.to_owned());
            names.push(format!("{name}_"));
            names.push(format!("clippy::{name}"));
            names.push(name.replace('_', "-"));
            names.push(name.to_uppercase());
            names.push(name[..name.len() - 1].to_owned());
        }
        names.extend(["", "_", "clippy", "warnings"].map(String::from));
        for name in &names {
            assert_eq!(named(name), named_by_scan(name), "{name:?}");
            let by_scan = lints().iter().find(|it| it.name == *name);
            assert_eq!(lint(name).map(|it| it as *const Info), by_scan.map(|it| it as *const Info));
            assert_eq!(lint_index_const(name), by_scan.map(Info::index), "{name:?}");
        }
    }

    #[test]
    fn indices_round_trip() {
        for (at, info) in lints().iter().enumerate() {
            assert_eq!(info.index().get(), at);
            assert!(std::ptr::eq(info.index().info(), info));
        }
    }
}
