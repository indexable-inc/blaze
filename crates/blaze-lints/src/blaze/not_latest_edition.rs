//! `not_latest_edition`: a package of the workspace that has not opted into the latest blaze
//! edition (docs/v2/plan/design/std.md, "The blaze edition: opting in"): one on a Rust
//! edition, or on an older blaze edition once there is more than one. One rule covers both:
//! the crate's blaze edition ([`base_db::Crate::blaze_edition`]) is not
//! [`BlazeEdition::LATEST`].
//!
//! It is about the package, so it says the same thing, with no place in a source file, from
//! whichever of the package's crates is linted: its library, or a binary or an example of a
//! package with no library of its own among that crate's dependencies. A client that shows
//! a build's diagnostics shows equal ones once, so a package with several binaries and no
//! library is still told once a build. Not found: a test or a bench (the package's library
//! or binary says it), a build script and a proc-macro crate (neither can be of a blaze
//! edition yet), and a dependency, a registry or git crate or the standard library, whose
//! lints are capped at `allow` as every lint's are. No fix: the migration, `blaze fix
//! --edition`, is planned.

use base_db::{CrateOrigin, TargetKind};
use blaze_diagnostics::model::MultiSpan;
use span::BlazeEdition;

use super::Lint;
use crate::check::Cx;

const LINT: Lint = Lint::NotLatestEdition;

/// What the latest blaze edition gives, in one clause.
const GIVES: &str = "`main` takes what it may touch (`fn main(cx: &Cx, io: Io)`) and `std` is \
                     bstd, blaze's capability-based standard library";

/// What the finding says of the package named `package`, on the edition `on` as its manifest
/// says it.
pub(super) fn message(package: &str, on: &str) -> String {
    let latest = BlazeEdition::LATEST.manifest();
    // alloc: a finding's message, once per package linted
    format!(
        "package `{package}` is on {on}, not the latest blaze edition `{latest}`, under which {GIVES}: \
         set `edition = \"{latest}\"` in its Cargo.toml"
    )
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || !cx.at_crate_root() {
        return;
    }
    let db = cx.db();
    let krate = cx.krate().base();
    let data = krate.data(db);
    let on = match krate.blaze_edition(db) {
        Some(BlazeEdition::LATEST) => return,
        // alloc: a finding's message
        Some(older) => format!("the blaze edition `{older}`"),
        // alloc: a finding's message
        None => format!("Rust edition {}", data.edition),
    };
    // A workspace member's or a path package's own crate: never a registry or git crate's,
    // nor the standard library's.
    let CrateOrigin::Local { name, .. } = &data.origin else { return };
    let shown = krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str());
    let Some(package) = name.as_ref().map(|it| it.as_str()).or(shown) else { return };
    let says = match data.target_kind {
        TargetKind::Lib { is_proc_macro } => !is_proc_macro,
        // A program says it for a package with no library: its own library is among its
        // dependencies when there is one.
        TargetKind::Bin | TargetKind::Example => {
            !data.dependencies.iter().any(|dep| dep.crate_id.data(db).origin == data.origin)
        }
        TargetKind::Test | TargetKind::Bench | TargetKind::BuildScript | TargetKind::Other => false,
    };
    if !says {
        return;
    }
    let message = message(package, &on);
    let root = cx.root();
    cx.blaze(LINT, root, MultiSpan::default(), message);
}

#[cfg(test)]
mod tests {
    use blaze_diagnostics::lints::registry::ready;

    use super::LINT;
    use crate::tests::{CLIPPY_OFF, lint};

    const TOLD: &str = "not the latest blaze edition `blaze-2026.11`";

    fn flags(flags: &str) -> String {
        format!("{CLIPPY_OFF} -A blaze::general {flags}")
    }

    /// One crate, `krate` as a fixture names it, with `source` as its root.
    fn package(krate: &str, source: &str) -> String {
        format!("//- minicore: sized\n//- /src/lib.rs {krate}\n{source}")
    }

    fn told(out: &str) -> usize {
        out.matches(TOLD).count()
    }

    /// A package on a Rust edition is told exactly once, as a warning, with the manifest line
    /// that opts in and what opting in gives; and its help names the lint's own switch.
    #[test]
    fn a_package_on_a_rust_edition_is_told_once() {
        let out = lint(
            &package("crate:app", "pub fn f() {}\npub fn g() { f() }\n"),
            &flags("--warn=blaze::not_latest_edition"),
        );
        assert_eq!(told(&out), 1, "{out}");
        assert!(out.contains("warning: package `app` is on Rust edition"), "{out}");
        assert!(out.contains("set `edition = \"blaze-2026.11\"` in its Cargo.toml"), "{out}");
        assert!(out.contains("fn main(cx: &Cx, io: Io)"), "{out}");
        assert!(out.contains(LINT.help()), "{out}");
    }

    /// Allowed at the crate root, or capped as a dependency's lints are, it says nothing;
    /// denied, it is an error that names the lint. (The first assertion of the test above is
    /// the control: the same crate with the lint at `warn` is told.)
    #[test]
    fn it_is_allowed_denied_and_capped_as_any_lint() {
        let source = "pub fn f() {}\n";
        let allowed = format!("#![allow(blaze::not_latest_edition)]\n{source}");
        let out = lint(&package("crate:app", &allowed), &flags("--warn=blaze::not_latest_edition"));
        assert_eq!(told(&out), 0, "{out}");
        let out = lint(
            &package("crate:app", source),
            &flags("--warn=blaze::not_latest_edition --cap-lints allow"),
        );
        assert_eq!(told(&out), 0, "a dependency is not told:\n{out}");
        let out = lint(&package("crate:app", source), &flags("--deny=blaze::not_latest_edition"));
        assert_eq!(told(&out), 1, "{out}");
        assert!(out.contains("error: package `app`"), "{out}");
        assert!(out.contains("blaze::not_latest_edition"), "the error names the lint:\n{out}");
    }

    /// With no level set it is told exactly when a package of the edition builds wherever
    /// blaze runs (`ready::EDITION_SHIPS`).
    #[test]
    fn by_default_it_waits_for_the_edition_to_ship() {
        let out = lint(&package("crate:app", "pub fn f() {}\n"), &flags(""));
        assert_eq!(told(&out), usize::from(ready::EDITION_SHIPS), "{out}");
    }

    /// A crate of the latest blaze edition is told nothing.
    #[test]
    fn a_package_on_the_latest_edition_is_told_nothing() {
        let out = lint(
            &package("crate:app", "#![blaze::edition = \"2026.11\"]\npub fn f() {}\n"),
            &flags("--warn=blaze::not_latest_edition"),
        );
        assert_eq!(told(&out), 0, "{out}");
    }

    /// A build script and a proc-macro crate, which cannot be of a blaze edition, say nothing.
    #[test]
    fn a_build_script_says_nothing() {
        let out = lint(
            &package("crate:custom-build:app", "fn main() {}\n"),
            &flags("--warn=blaze::not_latest_edition"),
        );
        assert_eq!(told(&out), 0, "{out}");
    }
}
