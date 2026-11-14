//! `blaze::trust`: a dependency (any crate that is not a workspace member) may not run `unsafe`
//! code or use an ambient capability (files, sockets, processes, the environment, the clock,
//! threads, global state, randomness, foreign code, a build script, a proc macro) unless the
//! workspace's root manifest grants it that capability by name, with a reason
//! (`[workspace.metadata.blaze.trust]`, [`grants::Grants`]). What a new version of a
//! dependency starts to do is denied until a person grants it: a compromised release that opens
//! a socket fails the build that pulls it in.
//!
//! - `dependency_capability` (deny): a dependency uses a capability it is not granted, or the
//!   workspace has no table at all (one finding, naming `blaze audit grants`).
//! - `dependency_unsafe` (deny): a dependency has `unsafe` code it is not granted.
//! - `dependency_capability_unreached` (warn): a dependency's code that nothing the workspace
//!   builds reaches yet uses a capability it is not granted: the next call may reach it.
//! - `trust_grant_unused` (warn): a grant names no dependency, a version the build does not
//!   have, or a capability its dependency no longer uses.
//! - `workspace_unsafe` (allow; warn in blaze's ratchet): a workspace crate has `unsafe` code
//!   and its root does not say why (`#![blaze::trust(unsafe, reason = "..")]`).
//!
//! What a dependency uses is read off the effect walk ([`facts`]); what the walk could not see
//! (an unresolved `dyn`, a body that does not lower, a foreign function without a model) counts
//! as every capability, and the finding says what could not be seen. The findings of the
//! workspace are given once, at its [`facts::representative`] crate; they run only when the
//! driver gives the lints the root manifest's grants (`Conf::trust`).
//!
//! The cost (docs/v2/plan/design/trust.md): a dependency's facts are tracked per crate and per
//! instance, so an edit of a workspace body reruns that body's walk and the breadth-first
//! pass over the cached edges of the instances the workspace reaches; nothing of a dependency
//! is recomputed.

pub mod capability;
pub mod facts;
pub mod grants;
pub mod table;

mod attribute;

#[cfg(test)]
mod tests;

use std::alloc::Global;

use blaze_diagnostics::model::MultiSpan;
use syntax::view::{self as ast, AstNode as _};

use self::{
    capability::{Capability, Caps},
    facts::{Dep, Workspace, caps_of},
    grants::{Grants, TABLE, same_name},
};
use super::Lint;
use crate::check::Cx;

/// The lints of the group, in the order a finding of each is given.
pub const LINTS: [Lint; 5] = [
    Lint::DependencyCapability,
    Lint::DependencyUnsafe,
    Lint::DependencyCapabilityUnreached,
    Lint::TrustGrantUnused,
    Lint::WorkspaceUnsafe,
];

/// One finding about the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub lint: Lint,
    pub message: String,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.at_crate_root() {
        return;
    }
    workspace_unsafe(cx);
    let Some(grants) = cx.conf().trust.as_ref() else { return };
    if !LINTS[..4].iter().any(|it| cx.on_blaze(*it)) {
        return;
    }
    let db = cx.db();
    if facts::representative(db) != Some(cx.krate().base()) {
        return;
    }
    let workspace = facts::workspace(db);
    let root = cx.root();
    for finding in findings(&workspace, grants) {
        if !cx.on_blaze(finding.lint) {
            continue;
        }
        let lint = finding.lint;
        cx.blaze(lint, root, MultiSpan::default(), finding.message)
            .help(lint.help())
            .help(format!("for further information visit {}", lint.docs_url()));
    }
}

/// The findings of `workspace` under `grants`, in order: per dependency by name and version,
/// what it uses that is not granted, then what its unreached code does; then the grants not
/// needed. Pure, so a test reads it without a build.
pub fn findings(workspace: &Workspace, grants: &Grants) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new_in(Global);
    let manifest = &grants.manifest;
    let Some(table) = grants.table.as_deref() else {
        let using: Vec<&Dep> = {
            let mut using = Vec::new_in(Global);
            using.extend(workspace.deps.iter().filter(|it| !it.uses.is_empty()));
            using
        };
        if let Some(first) = using.first() {
            let used = &first.uses[0];
            out.push(Finding {
                lint: Lint::DependencyCapability,
                message: format!(
                    "{manifest} has no `{TABLE}`, and {} of the workspace's dependencies use \
                     capabilities no dependency has unless it is granted them, `{}` first: it \
                     {} ({}): {}; run `blaze audit grants` for the table they need today, read \
                     it, write each `reason` and add it",
                    using.len(),
                    first.name,
                    used.capability.said(),
                    used.capability.word(),
                    used.witness,
                ),
            });
        }
        return out;
    };
    for dep in &workspace.deps {
        let grant = grants.of(&dep.name);
        let granted = match grant {
            Some(grant) if grant.covers(dep.version.as_deref()) => grant.allow,
            Some(_) | None => Caps::EMPTY,
        };
        let why_not = |capability: Capability| match grant {
            None => format!("`{TABLE}` in {manifest} has no entry for it"),
            Some(grant) if !grant.covers(dep.version.as_deref()) => format!(
                "its grant in {manifest} is for version {} only, and the build has {} now: read \
                 what the new version does before moving the pin",
                grant.version.as_deref().unwrap_or(""),
                version_said(dep),
            ),
            Some(_) => format!(
                "its grant in {manifest} does not allow `{}`",
                capability.word()
            ),
        };
        for used in dep.uses.iter().filter(|it| !granted.has(it.capability)) {
            let lint = match used.capability {
                Capability::Unsafe => Lint::DependencyUnsafe,
                _ => Lint::DependencyCapability,
            };
            out.push(Finding {
                lint,
                message: format!(
                    "dependency `{}`{} {} (`{}`), and {}: {}",
                    dep.name,
                    version_shown(dep),
                    used.capability.said(),
                    used.capability.word(),
                    why_not(used.capability),
                    used.witness,
                ),
            });
        }
        for used in dep.unreached.iter().filter(|it| !granted.has(it.capability)) {
            out.push(Finding {
                lint: Lint::DependencyCapabilityUnreached,
                message: format!(
                    "dependency `{}`{} has code that {} (`{}`), which nothing the workspace \
                     builds reaches yet, and {}: {}",
                    dep.name,
                    version_shown(dep),
                    used.capability.said(),
                    used.capability.word(),
                    why_not(used.capability),
                    used.witness,
                ),
            });
        }
    }
    for grant in table {
        let named: Vec<&Dep> = {
            let mut named = Vec::new_in(Global);
            named.extend(workspace.deps.iter().filter(|it| same_name(&it.name, &grant.name)));
            named
        };
        if named.is_empty() {
            out.push(Finding {
                lint: Lint::TrustGrantUnused,
                message: format!(
                    "{manifest} grants `{}`, which is no dependency of the workspace: remove the \
                     entry",
                    grant.name
                ),
            });
            continue;
        }
        let covered: Vec<&Dep> = {
            let mut covered = Vec::new_in(Global);
            covered.extend(named.iter().copied().filter(|it| grant.covers(it.version.as_deref())));
            covered
        };
        if covered.is_empty() {
            // The dependency's own findings say what it uses now; this says the pin is stale.
            out.push(Finding {
                lint: Lint::TrustGrantUnused,
                message: format!(
                    "{manifest} grants `{}` at version {} only, and the build has {}: the grant \
                     covers nothing",
                    grant.name,
                    grant.version.as_deref().unwrap_or(""),
                    version_said(named[0]),
                ),
            });
            continue;
        }
        let needed = covered
            .iter()
            .fold(Caps::EMPTY, |caps, dep| caps.union(caps_of(&dep.uses)).union(caps_of(&dep.unreached)));
        for capability in grant.allow.without(needed).iter() {
            out.push(Finding {
                lint: Lint::TrustGrantUnused,
                message: format!(
                    "{manifest} grants `{}` `{}`, which it does not use: remove `\"{}\"` from its \
                     `allow`",
                    grant.name,
                    capability.word(),
                    capability.word(),
                ),
            });
        }
    }
    out
}

/// ` 1.4.0`, or nothing when the build has no version for it.
fn version_shown(dep: &Dep) -> String {
    match dep.version.as_deref() {
        Some(version) => format!(" {version}"),
        None => String::new(),
    }
}

/// `1.4.0`, or "no version".
fn version_said(dep: &Dep) -> &str {
    dep.version.as_deref().unwrap_or("no version")
}

/// `workspace_unsafe`: a workspace crate with `unsafe` code whose root does not say why.
fn workspace_unsafe(cx: &mut Cx<'_, '_>) {
    const LINT: Lint = Lint::WorkspaceUnsafe;
    if !cx.on_blaze(LINT) {
        return;
    }
    let db = cx.db();
    let krate = cx.krate().base();
    if !krate.data(db).origin.is_local() {
        return;
    }
    let Some((_, witness)) = facts::anywhere(db, krate).iter().find(|it| it.0 == Capability::Unsafe)
    else {
        return;
    };
    let root = cx.root();
    if waived(root) {
        return;
    }
    let name = krate.extra_data(db).display_name.as_ref().map_or("", |it| it.crate_name().as_str());
    let message = format!(
        "crate `{name}` of the workspace runs `unsafe` code and its root does not say why: {witness}; \
         write `#![blaze::trust(unsafe, reason = \"..\")]` at its root"
    );
    cx.blaze(LINT, root, MultiSpan::default(), message)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// Whether the crate root `root` says why its crate runs `unsafe` code:
/// `#![blaze::trust(unsafe, reason = "..")]` with a reason that is not empty.
fn waived(root: ast::Node<'_>) -> bool {
    let Some(owner) = ast::AnyHasAttrs::cast(root) else { return false };
    ast::attrs_including_inner(&owner).any(|attr| {
        let text = attr.syntax().text().to_string();
        let text: String = text.chars().filter(|it| !it.is_whitespace()).collect();
        let Some(args) = text.strip_prefix("#![blaze::trust(") else { return false };
        let says_unsafe = args.split([',', ')']).any(|it| it == "unsafe");
        let reason = args.split_once("reason=\"").map(|(_, rest)| rest.split('"').next().unwrap_or(""));
        says_unsafe && reason.is_some_and(|it| !it.is_empty())
    })
}
