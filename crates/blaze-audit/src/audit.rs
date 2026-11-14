//! The whole audit of one workspace: its sources' pins, its locked packages against the
//! advisory database and the registry's yanked versions, under its settings, into one
//! [`Report`]. Pure: the driver reads the files, hashes the trees through [`Trees`] and the
//! index through [`Index`], and reads the clock once, for `today`.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::VecDeque;

use blaze_fetch::Digest;

use crate::advisory::{Database, Informational};
use crate::cargo_lock::{Lock, Package, Source};
use crate::date::Date;
use crate::pins::SourcePins;
use crate::policy::{Settings, Severity, judged};
use crate::report::{DatabaseFacts, Finding, PinnedFacts, Report};
use crate::source::{OutsidePath, PinRefused, Pinned, Trees, check};
use crate::version::{affected, fix};
use crate::yanked::{Yanked, yanked};
use crate::{NOTICE, STALE_ACCEPTANCE, UNMAINTAINED, UNSOUND, VULNERABLE, WEAK_SOURCE_PIN, YANKED};

/// The registries RustSec's advisories are of: crates.io, by either of its index urls.
pub const CRATES_IO: [&str; 2] = ["registry+https://github.com/rust-lang/crates.io-index", "sparse+https://index.crates.io/"];

/// The index files blaze has.
pub trait Index {
    /// The index file of `package` in the registry `registry` (as `Cargo.lock` names it), or
    /// `None` when blaze does not have it.
    fn file(&mut self, registry: &str, package: &str) -> Option<Vec<u8, Global>>;
}

/// The advisory database, as the driver found it.
pub enum DatabaseInput<'a> {
    /// No database is set: why, and how to set one.
    Unset(String),
    /// The database's tree is not what `blaze.lock` pins, or is not pinned: it is not read.
    Refused(PinRefusedDatabase),
    /// Read, after its tree matched its pin.
    Read { path: String, tree: Digest, database: &'a Database },
}

/// Why the database was not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PinRefusedDatabase {
    pub path: String,
    pub said: String,
}

/// Whether the advisory database at `path`, whose tree is `found`, is the one `blaze.lock`
/// pins; the driver checks this before it parses one advisory.
pub fn verify_database(pins: &SourcePins, path: &str, found: &Digest) -> Result<(), PinRefusedDatabase> {
    let refused = |said: String| PinRefusedDatabase { path: path.to_owned(), said };
    match &pins.advisories {
        None => Err(refused(format!(
            "the advisory database at `{path}` is not pinned in `blaze.lock` (rule `{WEAK_SOURCE_PIN}`): its tree is \
             {found}; run `blaze audit pin` to record it, and review the diff"
        ))),
        Some(pin) if &pin.tree != found => Err(refused(format!(
            "the advisory database at `{path}` has the tree {found}, not the {} `blaze.lock` pins (rule \
             `{WEAK_SOURCE_PIN}`): it changed since it was pinned; pin it again with `blaze audit pin` once the \
             change is known",
            pin.tree
        ))),
        Some(_) => Ok(()),
    }
}

/// What the audit reads.
pub struct Input<'a> {
    pub lock: &'a Lock,
    /// The workspace's members, by package name: where chains start. Empty: every package
    /// with no source.
    pub members: &'a [String],
    pub outside: &'a [OutsidePath],
    pub pins: &'a SourcePins,
    pub settings: &'a Settings,
    pub database: DatabaseInput<'a>,
    pub trees: &'a mut dyn Trees,
    /// `None` when blaze has no registry index here.
    pub index: Option<&'a mut dyn Index>,
    pub today: Date,
}

/// `name version`, as a chain names a package.
fn named(package: &Package) -> String {
    format!("{} {}", package.name, package.version)
}

/// The shortest chain from a member to each package: for each, its parent on that chain.
/// Breadth first from the members in name order over dependencies in (name, version,
/// source) order, so the chain does not depend on the order of the lock's packages.
fn parents(lock: &Lock, members: &[String]) -> Vec<Option<usize>, Global> {
    let mut parent: Vec<Option<usize>, Global> = Vec::with_capacity_in(lock.packages.len(), Global);
    parent.resize(lock.packages.len(), None);
    let mut seen: Vec<bool, Global> = Vec::with_capacity_in(lock.packages.len(), Global);
    seen.resize(lock.packages.len(), false);
    let mut roots: Vec<usize, Global> = Vec::new_in(Global);
    roots.extend((0..lock.packages.len()).filter(|&at| {
        let it = &lock.packages[at];
        it.source == Source::Path && (members.is_empty() || members.contains(&it.name))
    }));
    roots.sort_by(|a, b| {
        let (a, b) = (&lock.packages[*a], &lock.packages[*b]);
        (&a.name, &a.version).cmp(&(&b.name, &b.version))
    });
    let mut queue: VecDeque<usize, Global> = VecDeque::new_in(Global);
    for root in roots {
        seen[root] = true;
        queue.push_back(root);
    }
    // Bounded: each package enters the queue once.
    while let Some(at) = queue.pop_front() {
        for &dep in &lock.packages[at].deps {
            if !seen[dep] {
                seen[dep] = true;
                parent[dep] = Some(at);
                queue.push_back(dep);
            }
        }
    }
    parent
}

fn chain(lock: &Lock, parent: &[Option<usize>], at: usize) -> Vec<String, Global> {
    let mut out = Vec::new_in(Global);
    let mut here = Some(at);
    // Bounded: a parent is set once, from a package already seen, so the chain has no cycle
    // and at most one step a package.
    while let Some(it) = here
        && out.len() <= lock.packages.len()
    {
        out.push(named(&lock.packages[it]));
        here = parent[it];
    }
    out.reverse();
    out
}

/// The audit.
pub fn audit(input: Input<'_>) -> Report {
    let Input { lock, members, outside, pins, settings, database, trees, index, today } = input;
    let parent = parents(lock, members);
    let mut findings: Vec<Finding, Global> = Vec::new_in(Global);
    let mut not_checked: Vec<String, Global> = Vec::new_in(Global);
    let mut pinned_facts: Vec<PinnedFacts, Global> = Vec::new_in(Global);
    // The locked package a refusal is about: the least version of that name from that source,
    // so it does not depend on the order of the lock's packages.
    let find = |why: &PinRefused| {
        let from = |it: &Package| match (why, &it.source) {
            (
                PinRefused::Missing { url, commit, .. }
                | PinRefused::Mismatch { url, commit, .. }
                | PinRefused::Tree { url, commit, .. }
                | PinRefused::NoCommit { url, commit, .. },
                Source::Git { url: u, commit: c, .. },
            ) => u == url && c == commit,
            (PinRefused::NoChecksum { version, registry, .. }, Source::Registry { url, .. }) => {
                url == registry && &it.version.to_string() == version
            }
            _ => false,
        };
        (0..lock.packages.len())
            .filter(|&at| lock.packages[at].name == why.package() && from(&lock.packages[at]))
            .min_by(|a, b| lock.packages[*a].version.cmp(&lock.packages[*b].version))
    };

    // Sources.
    let (refused, pinned) = check(lock, outside, pins, settings, trees);
    for why in refused {
        let at = find(&why);
        let version = at.map(|it| lock.packages[it].version.to_string()).unwrap_or_default();
        let mut finding = Finding::new(Severity::Error, why.rule(), why.package(), &version, why.to_string());
        finding.title = match &why {
            PinRefused::Missing { .. } => "a git source pinned by its commit alone".to_owned(),
            PinRefused::Mismatch { .. } => "a git tree that is not its pin".to_owned(),
            PinRefused::Tree { .. } => "a git tree that has no digest".to_owned(),
            PinRefused::NoCommit { .. } => "a git source without a whole commit".to_owned(),
            PinRefused::NoChecksum { .. } => "a registry package without a checksum".to_owned(),
            PinRefused::OutsidePath { .. } => "a path dependency outside the workspace".to_owned(),
        };
        if let Some(at) = at {
            finding.chain = chain(lock, &parent, at);
        }
        findings.push(finding);
    }
    for pin in pinned {
        pinned_facts.push(match pin {
            Pinned::Git { package, url, commit, tree, verified } => {
                PinnedFacts { package, source: format!("git {url}#{commit}"), pin: tree.to_string(), verified }
            }
            Pinned::AllowedPath { package, path } => {
                PinnedFacts { package, source: path, pin: "allow-unpinned-paths".to_owned(), verified: false }
            }
        });
    }
    let unverified = pinned_facts.iter().filter(|it| it.source.starts_with("git ") && !it.verified).count();
    if unverified > 0 {
        not_checked.push(format!(
            "the trees of {unverified} pinned git sources, which are not on disk yet: the build verifies each when it fetches it"
        ));
    }

    // Advisories.
    let mut used: Vec<&str, Global> = Vec::new_in(Global);
    let facts = match database {
        DatabaseInput::Unset(why) => {
            not_checked.push(format!("advisories: {why}"));
            None
        }
        DatabaseInput::Refused(why) => {
            let mut finding = Finding::new(Severity::Error, WEAK_SOURCE_PIN, "advisory database", "", why.said);
            finding.title = "an advisory database that is not its pin".to_owned();
            findings.push(finding);
            not_checked.push(format!("advisories: the database at `{}` was not read, as it is not its pin", why.path));
            None
        }
        DatabaseInput::Read { path, tree, database } => {
            let mut elsewhere = 0usize;
            for (at, package) in lock.packages.iter().enumerate() {
                let from_crates_io = matches!(&package.source, Source::Registry { url, .. } if CRATES_IO.contains(&url.as_str()));
                if !from_crates_io {
                    elsewhere += usize::from(!database.of(&package.name).is_empty() && package.source != Source::Path);
                    continue;
                }
                for advisory in database.of(&package.name) {
                    if !affected(&advisory.patched_reqs, &advisory.unaffected_reqs, &package.version) {
                        continue;
                    }
                    let (rule, base) = match advisory.informational {
                        None => (VULNERABLE, Severity::Error),
                        Some(Informational::Unsound) => (UNSOUND, Severity::Error),
                        Some(Informational::Unmaintained) => (UNMAINTAINED, Severity::Warning),
                        Some(Informational::Notice) => (NOTICE, Severity::Warning),
                    };
                    let accept = settings.accepted(&advisory.id);
                    if accept.is_some() {
                        used.push(&advisory.id);
                    }
                    let (severity, expired) = judged(base, accept, today);
                    let version = package.version.to_string();
                    let mut said = match advisory.informational {
                        None => format!("`{} {version}` is vulnerable: {}", package.name, advisory.title),
                        Some(Informational::Unsound) => format!("`{} {version}` is unsound: {}", package.name, advisory.title),
                        Some(Informational::Unmaintained) => format!("`{}` is unmaintained: {}", package.name, advisory.title),
                        Some(Informational::Notice) => format!("`{}` has a notice: {}", package.name, advisory.title),
                    };
                    if !advisory.arch.is_empty() || !advisory.os.is_empty() {
                        said.push_str(&format!(
                            " (only on arch {} / os {}; the build's targets were not checked)",
                            if advisory.arch.is_empty() { "any".to_owned() } else { advisory.arch.join(", ") },
                            if advisory.os.is_empty() { "any".to_owned() } else { advisory.os.join(", ") },
                        ));
                    }
                    let mut finding = Finding::new(severity, rule, &package.name, &version, said);
                    finding.advisory = Some(advisory.id.clone());
                    finding.title = advisory.title.clone();
                    finding.chain = chain(lock, &parent, at);
                    if let Some((fixed, compatible)) = fix(&advisory.patched_reqs, &package.version) {
                        finding.compatible = compatible;
                        if compatible {
                            finding.change = Some(match finding.chain.as_slice() {
                                [member, _] => format!(
                                    "in the manifest of `{member}`, `{} = \"{fixed}\"`",
                                    package.name
                                ),
                                _ => format!(
                                    "only `Cargo.lock` moves: `{}` {version} → {fixed}, a compatible release; lock again",
                                    package.name
                                ),
                            });
                        }
                        finding.fix = Some(fixed.to_string());
                    }
                    finding.functions.extend(advisory.functions.iter().map(|(path, _)| path.clone()));
                    finding.url = advisory.url.clone();
                    finding.aliases.extend(advisory.aliases.iter().cloned());
                    finding.cvss = advisory.cvss.clone();
                    finding.date = Some(advisory.date.to_string());
                    finding.accepted = accept.cloned();
                    finding.expired = expired.map(|it| it.to_string());
                    findings.push(finding);
                }
            }
            if elsewhere > 0 {
                not_checked.push(format!(
                    "advisories of {elsewhere} packages from git or another registry that share a name with an advised crates.io crate: RustSec's advisories are of crates.io's"
                ));
            }
            let newest = database.newest;
            Some(DatabaseFacts {
                path,
                tree: tree.to_string(),
                advisories: database.advisories.len(),
                withdrawn: database.withdrawn.len(),
                newest: newest.map(|it| it.to_string()),
                age_days: newest.map(|it| today.ordinal() - it.ordinal()),
            })
        }
    };
    used.sort();
    used.dedup();
    for accept in &settings.accept {
        if used.binary_search(&accept.id.as_str()).is_err() && facts.is_some() {
            let mut finding = Finding::new(
                Severity::Warning,
                STALE_ACCEPTANCE,
                "",
                "",
                format!(
                    "`accept` has `{}` (until {}), which matches no locked package: remove it from `[workspace.metadata.blaze.audit]`",
                    accept.id, accept.until
                ),
            );
            finding.advisory = Some(accept.id.clone());
            finding.accepted = Some(accept.clone());
            findings.push(finding);
        }
    }

    // Yanked versions.
    match index {
        None => not_checked.push("yanked versions: no index".to_owned()),
        Some(index) => {
            let mut missing = 0usize;
            let mut first_why = None;
            for (at, package) in lock.packages.iter().enumerate() {
                let Source::Registry { url, .. } = &package.source else {
                    continue;
                };
                let said = match index.file(url, &package.name) {
                    None => Yanked::NotChecked("blaze has no index file of it".to_owned()),
                    Some(bytes) => yanked(&package.name, &package.version, &bytes),
                };
                match said {
                    Yanked::No => {}
                    Yanked::Yes => {
                        let version = package.version.to_string();
                        let mut finding = Finding::new(
                            Severity::Error,
                            YANKED,
                            &package.name,
                            &version,
                            format!(
                                "`{} {version}` is yanked from `{url}`: lock again to move off it",
                                package.name
                            ),
                        );
                        finding.title = "a yanked version".to_owned();
                        finding.chain = chain(lock, &parent, at);
                        findings.push(finding);
                    }
                    Yanked::NotChecked(why) => {
                        missing += 1;
                        first_why.get_or_insert_with(|| format!("`{} {}`: {why}", package.name, package.version));
                    }
                }
            }
            if let Some(first) = first_why {
                not_checked.push(format!("yanked versions of {missing} registry packages, the first {first}"));
            }
        }
    }
    not_checked.push("reachability: whether the build reaches an advisory's affected functions".to_owned());
    Report::new(today.to_string(), lock.packages.len(), facts, not_checked, pinned_facts, findings)
}
