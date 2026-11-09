//! blaze: which configurations of a package use each of its items, and the lints that says
//! (docs/v2/plan/design/multiconfig-check.md): `blaze::dead` (no configuration checked uses it, rustc's
//! `dead_code` over every configuration at once), `blaze::test_only` (only the tests do: a
//! `#[cfg(test)]` would leave it out of the rest) and `blaze::cfg_only` (only some features,
//! platforms or targets do, with the narrowest `#[cfg]` that names them exactly, when one
//! does).
//!
//! A configuration is one unit's crate: the lib, the lib's tests, a bin, an integration test,
//! under one set of features and one platform. Each is observed alone ([`observe`], the
//! dead-code pass's liveness and what it would report, run on the crate as that configuration
//! builds it); [`merge`] then joins the observations by where each item is written and decides
//! what each lint says. Everything [`merge`] reads is plain data, serializable, with no
//! database: a unit's observation can be kept with its record and merged with the others'
//! later, and what it gives (each [`Finding`] with the configurations it is about, as data)
//! can be written by any report.
//!
//! The levels are rustc's way: each lint warns by default (none is denied by default, so every
//! program that builds keeps building), `#[allow]`, `#[expect]` and the command line set them,
//! and the group `blaze::reach` sets all three (`reach = "deny"` in `[lints.blaze]` is the
//! strict mode). An item `dead_code` is allowed or expected at is left alone by all three.

use std::fmt;

use base_db::Db;
use blaze_mem::{Tag, frame};
use serde::{Deserialize, Serialize};

use crate::{
    Options,
    cx::{Cx, Files},
    model::{Applicability, Diag, Level, Span},
};

#[cfg(test)]
mod tests;

/// A cfg atom: `test`, `unix`, `feature = "serde"`, `target_os = "linux"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Atom {
    pub key: String,
    pub value: Option<String>,
}

impl Atom {
    pub fn flag(key: &str) -> Atom {
        Atom { key: key.to_owned(), value: None } // alloc: an atom, once per atom observed
    }

    pub fn key_value(key: &str, value: &str) -> Atom {
        Atom { key: key.to_owned(), value: Some(value.to_owned()) } // alloc: as above
    }

    /// How narrow a predicate on it reads, lowest first: `test`, then features, then the
    /// platform from its most to its least telling key.
    fn rank(&self) -> u8 {
        match self.key.as_str() {
            "test" => 0,
            "feature" => 1,
            "target_os" => 2,
            "target_family" | "unix" | "windows" => 3,
            "target_arch" => 4,
            "target_env" => 5,
            "target_pointer_width" => 6,
            _ => 7,
        }
    }
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            Some(value) => write!(f, "{} = {value:?}", self.key),
            None => f.write_str(&self.key),
        }
    }
}

/// A `#[cfg]` predicate that tells the configurations that use an item from the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Predicate {
    Atom(Atom),
    Not(Atom),
    Any(Vec<Atom>),
    All(Atom, Atom),
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Predicate::Atom(it) => write!(f, "{it}"),
            Predicate::Not(it) => write!(f, "not({it})"),
            Predicate::Any(them) => {
                f.write_str("any(")?;
                for (i, it) in them.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{it}")?;
                }
                f.write_str(")")
            }
            Predicate::All(a, b) => write!(f, "all({a}, {b})"),
        }
    }
}

/// A level as the reachability lints keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Lvl {
    Allow,
    Expect,
    Warn,
    Deny,
}

impl Lvl {
    fn silenced(self) -> bool {
        matches!(self, Lvl::Allow | Lvl::Expect)
    }
}

/// The levels at an item `dead_code` would report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Levels {
    pub dead_code: Lvl,
    pub dead: Lvl,
    pub test_only: Lvl,
    pub cfg_only: Lvl,
}

/// Where an item is written, the same in every configuration that has it: the file as
/// diagnostics show it ([`crate::emit::shown`], so a test build's crate root, which carries an
/// appended harness, is the lib's file), the bytes of its name, its kind and name, and how
/// many before it had all of those (two items one macro call makes).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key {
    pub file: String,
    pub lo: u32,
    pub hi: u32,
    /// Made by a macro: never given a fix.
    pub in_expansion: bool,
    pub kind: String,
    pub name: String,
    pub nth: u32,
}

/// One definition as one configuration sees it, at its index in [`Observation::items`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// None for what has no node (the impl a builtin derive makes).
    pub key: Option<Key>,
    /// Where a finding points: its name, else its header.
    pub span: Option<Span>,
    /// Where a `#[cfg]` goes (the start of the item, before its attributes), with the
    /// indentation of its line.
    pub insert: Option<(Span, String)>,
    /// rustc's `def_descr`: "function", "struct", "method".
    pub descr: String,
    pub name: String,
    /// This configuration uses it (rustc's liveness).
    pub used: bool,
    /// `dead_code` would report it here.
    pub reported: bool,
    /// The levels at it, when `reported`.
    pub levels: Option<Levels>,
    /// The definition it is in (an impl's item's impl, a variant's enum), by index.
    pub parent: Option<u32>,
    /// Whose finding says this one's: an inherent impl's item's self type, a variant's enum.
    pub owner: Option<u32>,
    /// The impls that go with a type or trait: a `#[cfg]` on it needs the same on them.
    pub companions: Vec<u32>,
    /// A `#[cfg]` can go on it: an item in a file as written (not of a macro), free in its
    /// module or in an inherent impl, or an impl.
    pub fixable: bool,
}

/// Code a `#[cfg]` leaves out of a configuration, with the names in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inactive {
    pub file: String,
    pub lo: u32,
    pub hi: u32,
    /// Its predicate, as written, with no whitespace.
    pub predicate: String,
    pub names: Vec<String>,
}

/// What one configuration says of its definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// How findings name it: "lib", "lib (test)", "bin `x`" (the driver's).
    pub label: String,
    /// The driver's: whose unit a finding of it is given as.
    pub unit: u32,
    /// Its cfg atoms, sorted.
    pub atoms: Vec<Atom>,
    /// The files of its modules, sorted.
    pub files: Vec<String>,
    pub inactive: Vec<Inactive>,
    pub items: Vec<Item>,
    /// (referrer, referred): what the code it does not use names, by index.
    pub refs: Vec<(u32, u32)>,
    /// Why its liveness is not known; no finding is given for its package then.
    pub unported: Option<String>,
}

impl Observation {
    pub fn is_test(&self) -> bool {
        self.atoms.iter().any(|it| it.key == "test" && it.value.is_none())
    }

    fn has(&self, atom: &Atom) -> bool {
        self.atoms.binary_search(atom).is_ok()
    }
}

/// What `krate`, as its configuration builds it, says of its definitions: the observation
/// [`merge`] joins with the other configurations'. `label` and `unit` are the caller's.
pub fn observe(db: &Db, krate: hir::Crate, files: Files<'_>, options: &Options, label: String, unit: u32) -> Observation {
    blaze_query::db::attach(db, || {
        frame::task(Tag::Diagnostics, |frame| {
            let cx = Cx::new(db, krate, files, frame).with_lints(&options.lints);
            let mut observation = crate::lints::observe(&cx);
            observation.label = label;
            observation.unit = unit;
            observation
        })
    })
}

/// Which of the three lints a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lint {
    Dead,
    TestOnly,
    CfgOnly,
}

impl Lint {
    pub fn name(self) -> &'static str {
        match self {
            Lint::Dead => "blaze::dead",
            Lint::TestOnly => "blaze::test_only",
            Lint::CfgOnly => "blaze::cfg_only",
        }
    }
}

/// What one lint says of one item, with the configurations it is about as data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub lint: Lint,
    pub key: Key,
    pub descr: String,
    pub name: String,
    pub span: Span,
    /// `Warn` or `Deny`.
    pub level: Lvl,
    /// The labels of the configurations that use it, and of those that have it and don't.
    pub used_in: Vec<String>,
    pub unused_in: Vec<String>,
    /// The narrowest `#[cfg]` that names exactly the configurations that use it.
    pub predicate: Option<Predicate>,
    /// The `#[cfg]`s to add (on the item, and on the impls that go with it).
    pub fix: Vec<(Span, String)>,
    pub applicability: Applicability,
    /// Why the fix may not be right, when it may not.
    pub unsure: Option<String>,
    /// Some configuration checked was a test build.
    pub tests_checked: bool,
    /// The unit it is given as: the first configuration's that has it.
    pub unit: u32,
}

/// The narrowest predicate over `configs`' atoms that holds in exactly those whose `used` is
/// set: one atom (or its negation), else `any` of one key's values, else `all` of two atoms;
/// none when no such simple predicate is exact. Only atoms some configuration has and another
/// lacks can tell them apart.
pub fn narrowest(configs: &[&Observation], used: &[bool]) -> Option<Predicate> {
    let exact = |holds: &dyn Fn(&Observation) -> bool| configs.iter().zip(used).all(|(&c, &u)| holds(c) == u);
    let mut atoms: Vec<&Atom> = configs.iter().flat_map(|it| &it.atoms).collect(); // alloc: once per finding
    atoms.sort_unstable_by(|a, b| (a.rank(), *a).cmp(&(b.rank(), *b)));
    atoms.dedup();
    atoms.retain(|atom| !configs.iter().all(|c| c.has(atom)));
    if let Some(atom) = atoms.iter().find(|atom| exact(&|c| c.has(atom))) {
        return Some(Predicate::Atom((*atom).clone())); // clone: the finding's predicate, once
    }
    if let Some(atom) = atoms.iter().find(|atom| exact(&|c| !c.has(atom))) {
        return Some(Predicate::Not((*atom).clone())); // clone: as above
    }
    let mut start = 0;
    while start < atoms.len() {
        let key = &atoms[start].key;
        let end = start + atoms[start..].iter().take_while(|it| it.key == *key).count();
        let values: Vec<&Atom> = atoms[start..end]
            .iter()
            .copied()
            .filter(|atom| configs.iter().zip(used).all(|(c, &u)| u || !c.has(atom)))
            .collect(); // alloc: once per key tried
        if values.len() >= 2 && exact(&|c| values.iter().any(|atom| c.has(atom))) {
            return Some(Predicate::Any(values.into_iter().cloned().collect())); // alloc: clone: the predicate, once
        }
        start = end;
    }
    // Pairs, for the few atoms a set of configurations usually differ in.
    const PAIRS_UP_TO: usize = 32;
    if atoms.len() <= PAIRS_UP_TO {
        for (i, a) in atoms.iter().enumerate() {
            for b in &atoms[i + 1..] {
                if exact(&|c| c.has(a) && c.has(b)) {
                    return Some(Predicate::All((*a).clone(), (*b).clone())); // clone: the predicate, once
                }
            }
        }
    }
    None
}

/// One item's place in one configuration.
#[derive(Clone, Copy)]
struct At {
    config: usize,
    item: usize,
}

/// What each lint says of the items of one package's `observations` (each a configuration
/// of it), sorted by where the items are written. None when a configuration's liveness is not
/// known ([`Observation::unported`]).
pub fn merge(observations: &[Observation]) -> Vec<Finding> {
    if observations.iter().any(|it| it.unported.is_some()) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let tests_checked = observations.iter().any(Observation::is_test);
    // Every keyed item of every configuration, grouped by key.
    let mut all: Vec<(&Key, At)> = Vec::new(); // alloc: once per package merged
    for (config, o) in observations.iter().enumerate() {
        for (item, it) in o.items.iter().enumerate() {
            if let Some(key) = &it.key {
                all.push((key, At { config, item }));
            }
        }
    }
    all.sort_by(|a, b| (a.0, a.1.config, a.1.item).cmp(&(b.0, b.1.config, b.1.item)));
    all.dedup_by(|a, b| a.0 == b.0 && a.1.config == b.1.config);
    let mut findings: Vec<(Finding, Vec<At>, Vec<bool>)> = Vec::new(); // alloc: once per package merged
    for group in all.chunk_by(|a, b| a.0 == b.0) {
        let ats: Vec<At> = group.iter().map(|it| it.1).collect(); // alloc: once per item of the package
        let item = |at: At| &observations[at.config].items[at.item];
        let Some(levels) = ats.iter().find_map(|&at| item(at).levels.filter(|_| item(at).reported)) else { continue };
        if levels.dead_code.silenced() {
            continue;
        }
        let used: Vec<bool> = ats.iter().map(|&at| item(at).used).collect(); // alloc: as above
        let using: Vec<&Observation> = ats.iter().zip(&used).filter(|it| *it.1).map(|it| &observations[it.0.config]).collect(); // alloc: as above
        let (lint, level) = if using.is_empty() {
            (Lint::Dead, if levels.dead.silenced() { Lvl::Allow } else { levels.dead.max(levels.dead_code) })
        } else if using.iter().all(|it| it.is_test()) {
            (Lint::TestOnly, levels.test_only)
        } else {
            (Lint::CfgOnly, levels.cfg_only)
        };
        if level.silenced() {
            continue;
        }
        let first = item(ats[0]);
        let span_at = ats
            .iter()
            .copied()
            .find(|&at| item(at).span.as_ref().is_some_and(|it| !crate::emit::names_text(&it.file)))
            .unwrap_or(ats[0]);
        let Some(span) = item(span_at).span.clone() else { continue }; // clone: the finding's span, once
        let label = |at: &At| observations[at.config].label.clone(); // clone: a label, once per finding
        let configs: Vec<&Observation> = ats.iter().map(|at| &observations[at.config]).collect(); // alloc: once per finding
        let predicate = match lint {
            Lint::Dead => None,
            Lint::TestOnly | Lint::CfgOnly => narrowest(&configs, &used),
        };
        let finding = Finding {
            lint,
            key: group[0].0.clone(), // clone: the finding's key, once
            descr: first.descr.clone(), // clone: once per finding
            name: first.name.clone(), // clone: once per finding
            span,
            level,
            used_in: ats.iter().zip(&used).filter(|it| *it.1).map(|it| label(it.0)).collect(), // alloc: once per finding
            unused_in: ats.iter().zip(&used).filter(|it| !*it.1).map(|it| label(it.0)).collect(), // alloc: once per finding
            predicate,
            fix: Vec::new(), // alloc: empty until a fix is made below
            applicability: Applicability::MaybeIncorrect,
            unsure: None,
            tests_checked,
            unit: observations[ats[0].config].unit,
        };
        findings.push((finding, ats, used));
    }
    drop_owned(observations, &mut findings);
    fix(observations, &mut findings);
    findings.into_iter().map(|it| it.0).collect()
}

/// Leaves out a finding whose owner (an inherent impl's item's self type, a trait's item's
/// trait, a variant's enum, a field's struct or variant) has one of the same lint about the
/// same configurations: what is said of the owner says it.
fn drop_owned(observations: &[Observation], findings: &mut Vec<(Finding, Vec<At>, Vec<bool>)>) {
    let find = |key: &Key, findings: &[(Finding, Vec<At>, Vec<bool>)]| {
        findings.binary_search_by(|it| it.0.key.cmp(key)).ok()
    };
    let owned: Vec<bool> = findings
        .iter()
        .map(|(finding, ats, _)| {
            ats.iter().any(|at| {
                let o = &observations[at.config];
                let Some(owner) = o.items[at.item].owner else { return false };
                let Some(key) = o.items.get(owner as usize).and_then(|it| it.key.as_ref()) else { return false };
                find(key, findings).is_some_and(|at| {
                    let theirs = &findings[at].0;
                    theirs.lint == finding.lint && theirs.used_in == finding.used_in && theirs.unused_in == finding.unused_in
                })
            })
        })
        .collect(); // alloc: once per package merged
    let mut i = 0;
    findings.retain(|_| {
        i += 1;
        !owned[i - 1]
    });
}

/// The `#[cfg]` fix of each finding that has a predicate, machine-applicable only when it is
/// sound in every configuration checked (nothing a configuration that does not use the item
/// keeps names it) and no code a `#[cfg]` leaves out of every checked configuration names it.
fn fix(observations: &[Observation], findings: &mut [(Finding, Vec<At>, Vec<bool>)]) {
    // Of each item or impl a fix puts a `#[cfg]` on, in each configuration that does not use
    // it, its index there: (config, item, finding).
    let mut roots: Vec<(usize, usize, usize)> = Vec::new(); // alloc: once per package merged
    for (at_finding, (finding, ats, used)) in findings.iter_mut().enumerate() {
        let Some(predicate) = &finding.predicate else { continue };
        if finding.key.in_expansion {
            continue;
        }
        let item = |at: &At| &observations[at.config].items[at.item];
        let Some(first) = ats.iter().find(|at| item(at).fixable && item(at).insert.is_some()) else { continue };
        let Some((at, indent)) = &item(first).insert else { continue };
        let mut parts = vec![(at.clone(), format!("#[cfg({predicate})]\n{indent}"))]; // alloc: clone: the fix, once per finding
        let mut whole = true;
        let mut these = Vec::new(); // alloc: once per finding with a fix
        for (at, &u) in ats.iter().zip(used.iter()) {
            if u {
                continue;
            }
            these.push((at.config, at.item, at_finding));
            for &companion in &item(at).companions {
                let c = &observations[at.config].items[companion as usize];
                these.push((at.config, companion as usize, at_finding));
                match (&c.insert, c.fixable) {
                    (Some((place, indent)), true) => parts.push((place.clone(), format!("#[cfg({predicate})]\n{indent}"))), // alloc: clone: as above
                    _ => whole = false,
                }
            }
        }
        if !whole {
            continue;
        }
        parts.sort_by(|a, b| (&a.0.file, a.0.lo).cmp(&(&b.0.file, b.0.lo)));
        parts.dedup_by(|a, b| a.0.file == b.0.file && a.0.lo == b.0.lo);
        finding.fix = parts;
        finding.applicability = Applicability::MachineApplicable;
        roots.extend(these);
    }
    // Code no checked configuration compiles that names the item: the fix may break the
    // configuration that does.
    let uncovered = uncovered(observations);
    for (finding, _, _) in findings.iter_mut() {
        if finding.fix.is_empty() {
            continue;
        }
        if let Some(it) = uncovered.iter().find(|it| it.names.binary_search(&finding.name).is_ok()) {
            finding.applicability = Applicability::MaybeIncorrect;
            // alloc: once per finding unsure
            finding.unsure = Some(format!(
                "code under `#[cfg({})]` at {}, which no configuration checked compiles, names `{}`; check that configuration too",
                it.predicate, it.file, finding.name
            ));
        }
    }
    // What a fix leaves out is sound only if every name of it that stays is left out too: a
    // fix found unsound is no longer applied, which can make another's unsound. Each round
    // takes at least one fix out, so there are at most as many rounds as fixes.
    for _ in 0..=findings.len() {
        let mut changed = false;
        for (config, o) in observations.iter().enumerate() {
            // Of each item here, the fixes still applied that put a `#[cfg]` on it.
            let mut fixes_of: Vec<Vec<usize>> = vec![Vec::new(); o.items.len()]; // alloc: once per configuration and round
            for &(c, item, finding) in &roots {
                if c == config && findings[finding].0.applicability == Applicability::MachineApplicable {
                    fixes_of[item].push(finding);
                }
            }
            let roots_here: Vec<bool> = fixes_of.iter().map(|it| !it.is_empty()).collect(); // alloc: as above
            let removed = closed(o, &roots_here);
            let mut unsound: Vec<(usize, u32)> = Vec::new(); // alloc: once per configuration and round
            for &(referrer, referred) in &o.refs {
                if removed.get(referrer as usize).copied().unwrap_or(true) {
                    continue;
                }
                // The fixes whose item or impl holds `referred`.
                let mut at = Some(referred);
                for _ in 0..o.items.len() {
                    let Some(here) = at else { break };
                    if let Some(fixes) = fixes_of.get(here as usize) {
                        unsound.extend(fixes.iter().map(|&finding| (finding, referrer)));
                    }
                    at = o.items.get(here as usize).and_then(|it| it.parent);
                }
            }
            for (finding, referrer) in unsound {
                let (finding, _, _) = &mut findings[finding];
                if finding.applicability != Applicability::MachineApplicable {
                    continue;
                }
                let r = &o.items[referrer as usize];
                finding.applicability = Applicability::MaybeIncorrect;
                // alloc: once per finding unsure
                finding.unsure = Some(format!(
                    "{} `{}`, which {} keeps, names it; the `#[cfg]` would break that build unless it goes there too",
                    r.descr, r.name, o.label
                ));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// `roots` and everything in one of them (an impl's items, an enum's variants).
fn closed(o: &Observation, roots: &[bool]) -> Vec<bool> {
    (0..o.items.len())
        .map(|item| {
            let mut at = Some(item as u32);
            for _ in 0..o.items.len() {
                let Some(here) = at else { break };
                if roots.get(here as usize).copied().unwrap_or(false) {
                    return true;
                }
                at = o.items.get(here as usize).and_then(|it| it.parent);
            }
            false
        })
        .collect() // alloc: once per configuration and round
}

/// The code a `#[cfg]` leaves out of every configuration checked that has its file.
fn uncovered(observations: &[Observation]) -> Vec<&Inactive> {
    let mut out: Vec<&Inactive> = Vec::new(); // alloc: once per package merged
    for o in observations {
        for it in &o.inactive {
            let everywhere = observations.iter().filter(|other| other.files.binary_search(&it.file).is_ok()).all(|other| {
                other.inactive.iter().any(|them| them.file == it.file && them.lo == it.lo && them.hi == it.hi)
            });
            if everywhere && !out.iter().any(|them| them.file == it.file && them.lo == it.lo && them.hi == it.hi) {
                out.push(it);
            }
        }
    }
    out
}

impl Finding {
    /// The finding as a diagnostic of its lint, at its level.
    pub fn diag(&self) -> Diag {
        let participle = match self.descr.as_str() {
            "struct" | "variant" => "constructed",
            "field" => "read",
            _ => "used",
        };
        let (descr, name) = (&self.descr, &self.name);
        let message = match (self.lint, &self.predicate) {
            (Lint::Dead, _) => format!("{descr} `{name}` is never {participle}"), // alloc: the edge
            (Lint::TestOnly, _) => format!("{descr} `{name}` is only {participle} by tests"), // alloc: the edge
            (Lint::CfgOnly, Some(predicate)) => format!("{descr} `{name}` is only {participle} when `{predicate}`"), // alloc: the edge
            (Lint::CfgOnly, None) => format!("{descr} `{name}` is only {participle} in some configurations"), // alloc: the edge
        };
        let mut diag = Diag::lint(self.lint.name(), message, self.span.clone()); // clone: the edge
        if self.level == Lvl::Deny {
            diag.level = Level::Error;
        }
        let list = |labels: &[String]| labels.join(", ");
        match self.lint {
            Lint::Dead => {
                diag.note(format!("no configuration checked uses it: {}", list(&self.unused_in))); // alloc: the edge
                if !self.tests_checked {
                    diag.help("the tests were not checked, and may use it: `blaze check --all-targets` checks them too");
                }
            }
            Lint::TestOnly | Lint::CfgOnly => {
                diag.note(format!("used in {}; not in {}", list(&self.used_in), list(&self.unused_in))); // alloc: the edge
            }
        }
        match (&self.predicate, self.fix.is_empty()) {
            (Some(predicate), false) => {
                diag.multipart_suggestion(
                    // alloc: the edge
                    format!("leave it out of the builds that don't use it with `#[cfg({predicate})]`"),
                    self.fix.clone(), // clone: the edge
                    self.applicability,
                );
            }
            (Some(predicate), true) => {
                // alloc: the edge
                diag.help(format!(
                    "`#[cfg({predicate})]` would leave it out of the builds that don't use it, with the impls that go with it"
                ));
            }
            (None, _) if self.lint != Lint::Dead => {
                diag.note("no simple `#[cfg]` names exactly the configurations that use it");
            }
            (None, _) => {}
        }
        if let Some(unsure) = &self.unsure {
            diag.note(unsure.clone()); // clone: the edge
        }
        diag
    }
}
