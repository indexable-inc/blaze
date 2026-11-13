//! `tmp_path`: a path under the system's temporary directory named outside the one helper,
//! `blaze_core::temp` (docs/lints.md#tmp_path). Test homes made there by hand leaked, one per
//! run, and a reboot wiped a round's notes kept there; `blaze_core::temp::TempDir` is a
//! directory of its own that is removed with all it holds when it drops.
//!
//! A finding is:
//!
//! - a string literal written in the file, raw or not, in code or a macro call's input, whose
//!   value starts with `/tmp`, `/private/tmp` or `/var/tmp` followed by its end or a `/`;
//! - a use of `std::env::temp_dir`, by name resolution (a call, or the function named as a
//!   value), outside the module `temp` of `blaze_core`.
//!
//! Tests are linted too: their homes are what leaked. The fix: a `blaze_core::temp::TempDir`
//! held as long as the directory is used (`TempDir::for_test(prefix)` in a test); a file that
//! must sit beside the system's temporary files takes its directory from
//! `blaze_core::temp::base()`. A literal that names the real `/tmp` as policy (a sandbox
//! profile, a mount table) is kept with `#[expect(blaze::tmp_path, reason = "..")]` on its item.

use hir_def::{hir::Expr, resolver::ValueNs};
use syntax::view as ast;

use super::{Finding, Lint, facts, gapped_literal, general_facts, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::TmpPath;

/// The system's temporary directories, as a path's first components.
const TEMPORARY: &[&str] = &["/tmp", "/private/tmp", "/var/tmp"];

/// The literals at `node`.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    gapped_literal::literals(cx, node, |cx, token, range| {
        let Some(dir) = temporary(cx.snippet(range)) else { return };
        let db = cx.db();
        if test_code::module_of(cx, node).is_some_and(|it| the_helper(db, it)) {
            return;
        }
        let message = format!("`{dir}` is named by hand: a directory made there outlives what made it");
        let span = cx.span(range);
        let at = token.parent().unwrap_or(node);
        cx.blaze(LINT, at, span, message)
            .note(
                "a home or scratch directory under the system's temporary directory leaks one per run, and a \
                 reboot wipes what was kept there",
            )
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    });
}

/// The uses of `std::env::temp_dir` in the body walked.
pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let db = cx.db();
    let owner = analysis.def();
    let Ok(def) = hir_def::DefWithBodyId::try_from(owner) else { return found };
    if the_helper(db, owner.module(db)) {
        return found;
    }
    let body = analysis.body();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Path(_)) {
            continue;
        }
        let Some(ValueNs::FunctionId(f)) = general_facts::value_of(db, def, body, id) else { continue };
        let f = hir::Function::from(f);
        if f.name(db).as_str() != "temp_dir"
            || general_facts::std_top(db, f.module(db)).is_none_or(|it| it.as_str() != "env")
        {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        // alloc: a finding's message, once per finding
        let message = "`std::env::temp_dir` is used outside `blaze_core::temp`".to_owned();
        // alloc: a finding's note
        let note = "a directory made under it by hand outlives what made it: `blaze_core::temp::TempDir` is one \
                    removed with what it holds when it drops"
            .to_owned();
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// Whether `module` is the one helper, `blaze_core::temp`, which alone names the system's
/// temporary directory (a socket's short `/tmp` among it).
fn the_helper(db: &base_db::Db, module: hir::Module) -> bool {
    facts::crate_is(db, module.krate(db), "blaze_core")
        && module.name(db).is_some_and(|it| it.as_str() == "temp")
}

/// The temporary directory the literal `source`, as written, starts its value with.
fn temporary(source: &str) -> Option<&'static str> {
    let open = source.find('"')?;
    let value = &source[open + 1..];
    TEMPORARY.iter().copied().find(|dir| {
        value.strip_prefix(dir).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with('/') || rest.starts_with('"') || rest.starts_with('#')
        })
    })
}

#[cfg(test)]
mod tests {
    use super::temporary;
    use crate::tests::lint;

    const FOUND: &str = "is named by hand";
    const CALLED: &str = "`std::env::temp_dir` is used outside `blaze_core::temp`";

    const STD: &str = r#"
//- minicore: sized, copy, clone, option, result, fn
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod env {
    pub struct PathBuf;
    pub fn temp_dir() -> PathBuf { PathBuf }
    pub fn current_dir() -> PathBuf { PathBuf }
}
"#;

    fn found(main: &str) -> String {
        let fixture = format!(
            "{}//- /src/lib.rs crate:main deps:std\n{}",
            STD.trim_start_matches('\n'),
            main.trim_start_matches('\n')
        );
        lint(&fixture, "-A clippy::all --warn=blaze::tmp_path")
    }

    /// The bug in miniature: a test's home made under `/tmp` by hand, a raw literal of one,
    /// one in a macro call's input, and the system's directory asked for by name.
    #[test]
    fn a_home_under_tmp_is_found() {
        let out = found(
            r#"
use std::env::temp_dir as scratch;
pub const HOME: &str = "/tmp/blaze-test-home";
pub fn homes() -> [&'static str; 2] {
    [r"/private/tmp", concat!("/var/tmp/", "x")]
}
pub fn dir() -> std::env::PathBuf {
    scratch()
}
pub fn named() -> fn() -> std::env::PathBuf {
    std::env::temp_dir
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 3, "{out}");
        assert_eq!(out.matches(CALLED).count(), 2, "the renamed import is resolved:\n{out}");
    }

    /// Not a finding: a path that only starts like one (`/tmpfs`, `/var/tmpl`), a relative
    /// `tmp`, and another function of `env`.
    #[test]
    fn other_paths_are_left_alone() {
        let out = found(
            r#"
pub const MOUNT: &str = "/tmpfs/blaze";
pub const OTHER: &str = "/var/tmpl";
pub const RELATIVE: &str = "tmp/out";
pub fn here() -> std::env::PathBuf {
    std::env::current_dir()
}
"#,
        );
        assert_eq!(out.matches(FOUND).count() + out.matches(CALLED).count(), 0, "{out}");
    }

    /// A literal that names the real `/tmp` as policy is kept by an expectation on its item.
    #[test]
    fn an_expectation_on_policy_is_fulfilled() {
        let out = found(
            r#"
#[expect(blaze::tmp_path, reason = "the sandbox profile denies writes under the real /tmp")]
pub const DENIED: &str = "/private/tmp";
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }

    #[test]
    fn the_value_is_read_past_its_prefix() {
        assert_eq!(temporary(r#""/tmp""#), Some("/tmp"));
        assert_eq!(temporary(r##"r#"/var/tmp/a"#"##), Some("/var/tmp"));
        assert_eq!(temporary(r#"b"/private/tmp/x""#), Some("/private/tmp"));
        assert_eq!(temporary(r#""/tmpx""#), None);
        assert_eq!(temporary(r#""a/tmp""#), None);
    }
}
