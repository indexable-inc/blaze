//! `env_read`: a read of the process's environment (`std::env::var`, `var_os`, `vars`,
//! `vars_os`) outside the modules that hold the workspace's settings.
//!
//! A setting read where it is used is a hidden input to whatever computes with it (AGENTS.md
//! rule keys-cover-every-input): no key says it, a test cannot set it without setting it for
//! the whole process, and two reads of one name may disagree on its default (rule no-fallback).
//! The whole environment (`vars`, `vars_os`) is every such input at once. Configuration is
//! typed and read once, at the entry point or in a settings module, and handed to the code that
//! needs it.
//!
//! This was `.clippy.toml`'s `disallowed-methods` ban of the four functions, at warn, where
//! about 170 reads stood unremarked. Here it is by the function a path or call resolves to
//! (through a `use`, an alias or a re-export, never by the text written: [`std_use::uses_in`]),
//! one finding per function per body at its first use, and the modules allowed to read are
//! named in `clippy.toml`'s `env-read-allowed` (each a module path from its crate's name,
//! `blaze_core::settings`; a module is allowed when it is one of them, not below one). Not
//! found: a build script (cargo hands it its inputs in the environment, and
//! `build_script_ambient` holds it), test code, `env!` and `option_env!` (read by the compile,
//! and keyed by it). No fix: what the setting's type is and who hands it on is a decision.
//!
//! The instance: `std::env::vars_os()` copied in four places of blaze-cargo's command line
//! (cli.rs, cli/clean.rs, cli/own.rs) and blaze-thin's, each its own filter of what is UTF-8,
//! where cli/own.rs's `env()` is the one home.

use base_db::TargetKind;
use syntax::{NodeOrToken, TextRange};

use super::{Lint, std_use, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::EnvRead;

/// The functions read, by the name [`std_use::uses_in`] gives them.
pub(crate) const READS: [&str; 4] = ["std::env::var", "std::env::var_os", "std::env::vars", "std::env::vars_os"];

/// The `clippy.toml` key naming the modules that may read the environment.
pub(crate) const ALLOWED_KEY: &str = "env-read-allowed";

/// What every finding says.
const WHY: &str = "a setting read where it is used is an input no key says and no test can set \
                   for one call: read it once, typed, at the entry point or in a settings module, \
                   and hand it on";

/// The body's module, from its crate's name: `blaze_cargo::cli::own`.
fn module_path(cx: &Cx<'_, '_>, module: hir::Module) -> String {
    let db = cx.db();
    // alloc: a module's path, once per body that reads the environment
    let mut path = String::new();
    if let Some(krate) = module.krate(db).display_name(db) {
        path.push_str(krate.crate_name().as_str());
    }
    for segment in module.path_segments(db) {
        path.push_str("::");
        path.push_str(segment.as_str());
    }
    path
}

/// Whether `module` is one `allowed` names: exactly, never a module below one.
pub(crate) fn is_allowed(module: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|it| it == module)
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    let db = cx.db();
    if matches!(test_code::target_of(db, cx.krate()), TargetKind::BuildScript | TargetKind::Test | TargetKind::Bench) {
        return;
    }
    let (mut found, module) = {
        let Some(analysis) = cx.analysis() else { return };
        let found = std_use::uses_in(db, analysis, &|_| "");
        (found, analysis.def().module(db))
    };
    found.retain(|it| READS.contains(&it.name.as_str()));
    if found.is_empty() {
        return;
    }
    let module = module_path(cx, module);
    let allowed = cx.conf().strings(ALLOWED_KEY).unwrap_or_default();
    if is_allowed(&module, &allowed) {
        return;
    }
    found.sort_by_key(|it| it.first.map(TextRange::start));
    for used in found {
        let Some(range) = used.first else { continue };
        let root = cx.root();
        let at = match root.text_range().contains_range(range) {
            true => match root.covering_element(range) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => it.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(range);
        cx.blaze(LINT, at, span, message(&used.name, used.count, &module))
            .note(WHY)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The finding's first line: the function, how often the body reads it, and the module, which
/// is the name `env-read-allowed` would take.
pub(crate) fn message(name: &str, count: u32, module: &str) -> String {
    let times = match count {
        1 => "once".to_owned(), // alloc: a message's part
        n => format!("{n} times"),
    };
    format!("`{name}`, read {times} in `{module}`, which is not a settings module (`{ALLOWED_KEY}` in clippy.toml)")
}
