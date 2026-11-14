//! `untied_child`: test code that starts a process whose `std::process::Child` no guard owns,
//! or that runs `blaze` without `BLAZE_DAEMON_EXIT_UNHELD` on its command.
//!
//! The rule: a process a test starts ends with the test. A `Child` dropped is neither killed
//! nor waited for, so a test that panics or fails before its `wait` leaves the process running;
//! and a daemon a test's `blaze` starts outlives even a test that ends well, unless it is told
//! to exit once nothing holds it (`BLAZE_DAEMON_EXIT_UNHELD`, crates/blaze-cargo/src/serve/
//! unheld.rs `VAR`). 44 daemons ran at once and 8 were orphaned when tests were killed at
//! their time cap.
//!
//! The guard is `stdx::JodChild`, which kills and reaps its child on drop. A finding is, in test
//! code (a body under `#[test]` or `#[cfg(test)]`, or any body of an integration test target):
//!
//! - a call whose value is a `Child` or a `Result` of one: `std::process::Command::spawn`, by
//!   name resolution, or a function of the tree that wraps it (one whose return type is a
//!   `Child` or a `Result` of one), unless its value goes straight into a `JodChild`
//!   (`JodChild(cmd.spawn()?)`, `cmd.spawn().map(JodChild)`, through `?`, `unwrap`, `expect`,
//!   `map_err` and `context`) or is waited for there and then (`cmd.spawn()?.wait()`, which is
//!   `status()` in two calls). A wrapper's own body is not linted: its callers are, as of a
//!   spawn. A `Child` kept in a struct whose hand-written `Drop` kills it is a finding too: the
//!   field is a `JodChild` instead (one guard, rule one-concept-one-home).
//! - a `Command::new(env!("CARGO_BIN_EXE_blaze"))` in a body that sets no
//!   `BLAZE_DAEMON_EXIT_UNHELD` on a command. The body is taken to set it when it calls
//!   `Command::env` with that key or a key that is not a literal (a constant, a field), calls
//!   `Command::envs`, hands a `Command` to a function of the tree (a helper that sets the
//!   environment), or returns a `Command` (its callers set it). `status()` and `output()` wait
//!   for their child, but a daemon the child started does not end with it, so this half applies
//!   to every way a command is run.
//!
//! Not found: a path to blaze held in a variable or a field (`Command::new(&self.blaze)`): what
//! it names is known only at run time. The fix, `MaybeIncorrect` where the crate depends on
//! `stdx` (a use of the `Child` by value, `wait_with_output`, then needs `into_inner`): the
//! value wrapped, `.map(stdx::JodChild)` on a `Result`, `stdx::JodChild(..)` on a `Child`.

use base_db::{Db, TargetKind};
use hir_def::{
    DefWithBodyId, FunctionId,
    expr_store::Body,
    hir::{Expr, ExprId, Literal},
    signatures::FunctionSignature,
};
use syntax::TextRange;

use super::{Finding, Lint, alloc::written_callee, facts, test_code};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::UntiedChild;

/// The variable that has a daemon exit once no client holds it: crates/blaze-cargo/src/serve/
/// unheld.rs `VAR`.
const UNHELD: &str = "BLAZE_DAEMON_EXIT_UNHELD";

/// How a test names the blaze it runs, as written, spaces taken out: cargo's path of the
/// package's binary `blaze`.
const BLAZE_BIN: &str = "env!(\"CARGO_BIN_EXE_blaze\")";

/// Methods that hand on the `Child` they are called on (in its `Result`), so a guard after them
/// still owns the spawn's child.
const PASSES: &[&str] =
    &["unwrap", "expect", "unwrap_or_else", "map_err", "context", "with_context", "wrap_err", "wrap_err_with"];

/// Methods of `Child` that wait for it there and then.
const WAITS: &[&str] = &["wait", "wait_with_output"];

/// How a call's value holds its `Child`.
#[derive(Clone, Copy)]
enum Held {
    Bare,
    InResult,
}

/// The fix of a finding: what to write where.
struct Fix {
    parts: Vec<(TextRange, &'static str)>,
}

/// A body's findings, and the fixes of those that have one, by the finding's range.
struct Found {
    findings: Vec<Finding>,
    fixes: Vec<(TextRange, Fix)>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Found { findings, mut fixes } = find(cx);
    for finding in findings {
        let fix = fixes.iter().position(|(at, _)| *at == finding.range).map(|it| fixes.swap_remove(it).1);
        let parts: Option<Vec<_>> = fix.map(|fix| {
            // alloc: the fix's few parts
            fix.parts.into_iter().map(|(at, text)| (cx.span(at), text.to_owned())).collect()
        });
        let diagnostic = facts::report_one(cx, finding);
        if let Some(parts) = parts {
            diagnostic.multipart_suggestion(
                "own the child with `stdx::JodChild`, which kills and reaps it on drop",
                parts,
                Applicability::MaybeIncorrect,
            );
        }
    }
}

fn find(cx: &Cx<'_, '_>) -> Found {
    // alloc: the findings of one body, reported at the edge
    let mut found = Found { findings: Vec::new(), fixes: Vec::new() };
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let integration = matches!(test_code::target_of(db, cx.krate()), TargetKind::Test);
    if !integration && !cx.is_in_test(cx.root()) {
        return found;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    let returns = match def {
        DefWithBodyId::FunctionId(f) => Some(hir::Function::from(f).ret_type(db)),
        _ => None,
    };
    // A wrapper of a spawn: its callers are linted, not its body.
    let wrapper = returns.as_ref().is_some_and(|ty| held(db, ty).is_some());
    let stdx = has_stdx(db, cx.krate());
    let mut parents = None;
    let mut daemon = Daemon {
        // alloc: the few blaze commands of one body
        starts: Vec::new(),
        set: returns.as_ref().is_some_and(|ty| ty.as_adt().is_some_and(|it| is_process(db, it, "Command"))),
    };
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((f, _)) = written_callee(infer, body, id) else { continue };
        daemon.see(cx, analysis, body, id, f);
        if wrapper {
            continue;
        }
        let Some((what, holds)) = spawns(cx, analysis, id, f) else { continue };
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        if tied(cx, analysis, body, parents, id) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let message = format!(
            "the `Child` of {what} is owned by no `stdx::JodChild`: a test that panics or fails before it waits leaves the process running"
        );
        // alloc: a message's part
        let note = "a dropped `Child` is neither killed nor waited for, and a test that fails on an assert or \
                    an `unwrap` unwinds past its `kill`: hold the value in `stdx::JodChild` \
                    (`stdx::JodChild::spawn(command)`), which kills and reaps it on drop, and a struct's field \
                    as a `JodChild`, not a `Child` with a hand-written `Drop`"
            .to_owned();
        let written = cx.snippet(range).trim_end().ends_with(')');
        let fix = (stdx && written).then(|| Fix {
            parts: match holds {
                // alloc: the fix's one part
                Held::InResult => vec![(TextRange::empty(range.end()), ".map(stdx::JodChild)")],
                // alloc: the fix's two parts
                Held::Bare => vec![
                    (TextRange::empty(range.start()), "stdx::JodChild("),
                    (TextRange::empty(range.end()), ")"),
                ],
            },
        });
        facts::push(&mut found.findings, LINT, range, message, Some(note));
        if let Some(fix) = fix {
            found.fixes.push((range, fix));
        }
    }
    if !daemon.set {
        for range in daemon.starts {
            // alloc: a message's part
            let message = format!(
                "a test runs `blaze` with no `{UNHELD}` set on its command: the daemon it starts outlives a test killed at its time cap"
            );
            // alloc: a message's part
            let note = format!(
                "set `.env(\"{UNHELD}\", \"on\")` on the command (crates/blaze-cargo/src/serve/unheld.rs): the daemon \
                 then exits once no client holds a connection to it, however the test ended"
            );
            facts::push(&mut found.findings, LINT, range, message, Some(note));
        }
    }
    found
}

/// What the call `id` of `f` starts, as a finding names it, and how its value holds the
/// `Child`, when it is a spawn or a wrapper of one.
fn spawns(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    id: ExprId,
    f: FunctionId,
) -> Option<(String, Held)> {
    let db = cx.db();
    let holds = held(db, &analysis.ty(analysis.infer().expr_ty(id)))?;
    let krate = hir::Function::from(f).module(db).krate(db);
    let adt = facts::impl_adt(db, f);
    let name = FunctionSignature::of(db, f).name.as_str();
    if ["std", "core", "alloc"].iter().any(|it| facts::crate_is(db, krate, it)) {
        // alloc: a message's part
        return (name == "spawn" && adt.is_some_and(|it| is_process(db, it, "Command")))
            .then(|| ("`Command::spawn`".to_owned(), holds));
    }
    if adt.is_some_and(|it| facts::adt_is(db, it, "stdx", "JodChild")) {
        // `into_inner`: the guard taken off on purpose.
        return None;
    }
    // alloc: a message's part
    Some((format!("`{name}`"), holds))
}

/// Whether the value of `id` goes straight into a `JodChild`, or is waited for there and then.
fn tied(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    body: &Body,
    parents: &facts::Parents,
    id: ExprId,
) -> bool {
    let db = cx.db();
    let infer = analysis.infer();
    let guards = |at: ExprId| {
        facts::holds(db, &analysis.ty(infer.expr_ty(at)), 1, &mut |adt| {
            facts::adt_is(db, adt, "stdx", "JodChild")
        })
        .is_some()
    };
    let mut at = id;
    // A few steps: `?` or `unwrap`, then the guard.
    for _ in 0..8 {
        let Some(up) = parents.of_expr(at) else { return false };
        match body.expr(up) {
            Expr::MethodCall { receiver, method_name, .. } if receiver == at => {
                let name = method_name.as_str();
                if PASSES.contains(&name) {
                    at = up;
                    continue;
                }
                return WAITS.contains(&name) || (name == "map" && guards(up));
            }
            Expr::Call { args, .. } if args.len() == 1 && args[0] == at => {
                let branch = written_callee(infer, body, up).is_some_and(|(f, _)| {
                    FunctionSignature::of(db, f).name.as_str() == "branch"
                        && facts::crate_is(db, hir::Function::from(f).module(db).krate(db), "core")
                });
                if !branch {
                    return guards(up);
                }
                // `?`, lowered into a match on `Try::branch`: the match is the value.
                let Some(matched) = parents.of_expr(up) else { return false };
                at = matched;
            }
            _ => return false,
        }
    }
    false
}

/// The blaze commands a body makes, and whether it sets [`UNHELD`], or may.
struct Daemon {
    starts: Vec<TextRange>,
    set: bool,
}

impl Daemon {
    fn see(
        &mut self,
        cx: &Cx<'_, '_>,
        analysis: &hir::BodyAnalysis<'_>,
        body: &Body,
        id: ExprId,
        f: FunctionId,
    ) {
        let db = cx.db();
        let (receiver, args) = match body.expr(id) {
            Expr::MethodCall { receiver, args, .. } => (Some(receiver), args),
            Expr::Call { args, .. } => (None, args),
            _ => return,
        };
        let krate = hir::Function::from(f).module(db).krate(db);
        if !facts::crate_is(db, krate, "std") {
            // A helper of the tree handed the command may set it.
            let infer = analysis.infer();
            let command = |it: &ExprId| {
                analysis
                    .ty(infer.expr_ty(*it))
                    .strip_references()
                    .as_adt()
                    .is_some_and(|adt| is_process(db, adt, "Command"))
            };
            self.set |= receiver.iter().chain(args).any(command);
            return;
        }
        if !facts::impl_adt(db, f).is_some_and(|it| is_process(db, it, "Command")) {
            return;
        }
        // The arguments after the command, written as a method's or as a path's call.
        let after = match receiver {
            Some(_) => args,
            None => args.get(1..).unwrap_or_default(),
        };
        match FunctionSignature::of(db, f).name.as_str() {
            "new" => {
                let Some(&program) = args.first() else { return };
                let Some(range) = analysis.expr_range(program) else { return };
                let written: String = cx.snippet(range).chars().filter(|it| !it.is_whitespace()).collect();
                if written.ends_with(BLAZE_BIN) {
                    let Some(at) = analysis.expr_range(id) else { return };
                    self.starts.push(at);
                }
            }
            "env" => {
                let Some(&key) = after.first() else { return };
                self.set |= match body.expr(key) {
                    Expr::Literal(Literal::String(it)) => it.as_str() == UNHELD,
                    _ => true,
                };
            }
            "envs" => self.set = true,
            _ => {}
        }
    }
}

/// How `ty` holds a `std::process::Child`: it is one, or a `Result` of one.
fn held(db: &Db, ty: &hir::Type<'_>) -> Option<Held> {
    let adt = ty.as_adt()?;
    if is_process(db, adt, "Child") {
        return Some(Held::Bare);
    }
    let first = facts::adt_is(db, adt, "core", "Result").then(|| ty.type_arguments().next()).flatten()?;
    first.as_adt().is_some_and(|it| is_process(db, it, "Child")).then_some(Held::InResult)
}

/// Whether `adt` is std's `process::<name>`.
fn is_process(db: &Db, adt: hir::Adt, name: &str) -> bool {
    let module = adt.module(db);
    facts::adt_is(db, adt, "std", name) && module.name(db).is_some_and(|it| it.as_str() == "process")
}

/// Whether `krate` can name `stdx::JodChild`: a dependency named `stdx`.
fn has_stdx(db: &Db, krate: hir::Crate) -> bool {
    krate
        .base()
        .data(db)
        .dependencies
        .iter()
        .any(|dep| facts::crate_is(db, hir::Crate::from(dep.crate_id), "stdx"))
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const UNTIED: &str = "is owned by no `stdx::JodChild`";
    const UNHELD: &str = "with no `BLAZE_DAEMON_EXIT_UNHELD` set on its command";

    /// A `std` with `process::Command` and `Child` by the paths the real ones have, and a
    /// `stdx` with the guard.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn, try, from, drop, env
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod io {
    pub struct Error;
    pub type Result<T> = core::result::Result<T, Error>;
}
pub mod process {
    pub struct ExitStatus(i32);
    pub struct Child(u32);
    impl Child {
        pub fn wait(&mut self) -> crate::io::Result<ExitStatus> { Ok(ExitStatus(0)) }
        pub fn kill(&mut self) -> crate::io::Result<()> { Ok(()) }
    }
    pub struct Command(u32);
    impl Command {
        pub fn new(_program: &str) -> Command { Command(0) }
        pub fn arg(&mut self, _arg: &str) -> &mut Command { self }
        pub fn env(&mut self, _key: &str, _value: &str) -> &mut Command { self }
        pub fn spawn(&mut self) -> crate::io::Result<Child> { Ok(Child(0)) }
        pub fn status(&mut self) -> crate::io::Result<ExitStatus> { Ok(ExitStatus(0)) }
    }
}
//- /stdx/lib.rs crate:stdx deps:std
pub struct JodChild(pub std::process::Child);
impl JodChild {
    pub fn spawn(mut command: std::process::Command) -> std::io::Result<JodChild> {
        Ok(JodChild(command.spawn()?))
    }
}
"#;

    fn found(main: &str) -> String {
        let fixture = format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::untied_child")
    }

    /// The bug in miniature: a test's child kept bare, by a spawn and by the test's own
    /// wrapper of one, and a blaze run with nothing to end its daemon.
    #[test]
    fn a_bare_child_and_a_blaze_with_no_unheld_are_found() {
        let out = found(
            r#"
//- /src/lib.rs crate:main deps:std,stdx cfg:test
#[cfg(test)]
mod tests {
    use std::process::{Child, Command};
    fn start(script: &str) -> Child {
        Command::new("/bin/sh").arg(script).spawn().unwrap_or_else(|_| loop {})
    }
    #[test]
    fn runs() -> std::io::Result<()> {
        let mut child = Command::new("/bin/sh").spawn()?;
        let _started = start("exit 0");
        child.kill()?;
        Command::new(env!("CARGO_BIN_EXE_blaze")).arg("build").status()?;
        Ok(())
    }
}
"#,
        );
        assert_eq!(out.matches(UNTIED).count(), 2, "{out}");
        assert!(out.contains("the `Child` of `Command::spawn`"), "{out}");
        assert!(out.contains("the `Child` of `start`"), "{out}");
        assert!(out.contains(".map(stdx::JodChild)"), "{out}");
        assert_eq!(out.matches(UNHELD).count(), 1, "{out}");
    }

    /// The fix: the child in a `JodChild`, by its `spawn` or wrapped, a spawn waited for at
    /// once, and a blaze run with the variable set (by a literal or a constant) or handed to a
    /// helper, are left alone; so is code outside tests.
    #[test]
    fn a_guarded_child_and_a_blaze_told_to_exit_unheld_are_left_alone() {
        let out = found(
            r#"
//- /src/lib.rs crate:main deps:std,stdx cfg:test
use std::process::Command;
pub fn outside() -> std::io::Result<()> {
    let _child = Command::new("/bin/sh").spawn()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use std::process::Command;
    use stdx::JodChild;
    const UNHELD: (&str, &str) = ("BLAZE_DAEMON_EXIT_UNHELD", "on");
    fn environ(command: &mut Command) {
        command.env(UNHELD.0, UNHELD.1);
    }
    #[test]
    fn runs() -> std::io::Result<()> {
        let _a = JodChild::spawn(Command::new("/bin/sh"))?;
        let _b = JodChild(Command::new("/bin/sh").spawn()?);
        Command::new("/bin/sh").spawn()?.wait()?;
        Command::new(env!("CARGO_BIN_EXE_blaze")).env("BLAZE_DAEMON_EXIT_UNHELD", "on").status()?;
        Ok(())
    }
    #[test]
    fn helper_sets_it() -> std::io::Result<()> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_blaze"));
        environ(&mut command);
        command.status()?;
        Ok(())
    }
}
"#,
        );
        assert_eq!(out.matches(UNTIED).count(), 0, "{out}");
        assert_eq!(out.matches(UNHELD).count(), 0, "{out}");
    }

    /// Every body of an integration test target is test code, helpers too; an expectation on a
    /// finding is met and says nothing.
    #[test]
    fn an_integration_test_helper_is_found_and_an_expect_is_met() {
        let out = found(
            r#"
//- /src/lib.rs crate:test:main deps:std,stdx
use std::process::Command;
pub fn helper() -> std::io::Result<()> {
    let _child = Command::new("/bin/sh").spawn()?;
    Ok(())
}
#[expect(blaze::untied_child, reason = "the fixture's control: an expectation met")]
pub fn expected() -> std::io::Result<()> {
    let _child = Command::new("/bin/sh").spawn()?;
    Ok(())
}
"#,
        );
        assert_eq!(out.matches(UNTIED).count(), 1, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }
}
