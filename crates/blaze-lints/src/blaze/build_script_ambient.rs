//! `build_script_ambient`: a build script of the workspace (`build.rs`, or the file the
//! manifest's `build =` names: the crate cargo calls `custom-build`) that uses the standard
//! library's ambient authority, reaching by itself for
//!
//! - the file system: a function of `std::fs`, or of its `File`, `OpenOptions`, `DirBuilder`,
//!   `ReadDir` or `DirEntry`;
//! - the environment: `std::env`'s `var`, `var_os`, `vars`, `vars_os`, `set_var`, `remove_var`,
//!   `current_dir`, `set_current_dir`, `args`, `args_os`, `current_exe`, `temp_dir`, `home_dir`;
//! - processes: `std::process`'s `Command` or `Child`, `exit`, `abort`, `id`;
//! - the network: `std::net`'s `TcpStream`, `TcpListener`, `UdpSocket`;
//! - the clock: `Instant::now` and `elapsed`, `SystemTime::now` and `elapsed`.
//!
//! Such a script cannot be analysed: blaze must run it in a sandbox, trace what it read, and
//! rerun it whenever anything it might have read changes. A script that is handed a capability
//! (docs/v2/plan/design/std.md: `Fs`, `Env`, `Process`, `Net`, `Clock`) states in its signature
//! what it can touch.
//!
//! This is `blaze::std_use` over a build script: the one table of what blaze's standard library
//! replaces ([`std_use::TABLE`]) and the one walk that resolves a body's uses to its rows
//! ([`std_use::uses_in`]), here the rows that are an [`Authority`], and reported once per
//! script, not once per function, since it is the script's whole reach that keeps blaze from
//! analysing it. `std_use` itself says nothing of a build script.
//!
//! A use is a path that names the function, called or not (`std::fs::read(p)`,
//! `.map(std::fs::read)`, `File::open(p)`), or a method call (`cmd.status()`,
//! `file.write_all(..)`), found by the function it resolves to: through a `use`, an alias, a
//! glob or a re-export, never by the text written. One finding per distinct item per script (a
//! free function, the type for a type's functions, or the function for a row that names
//! functions of a type), at its first use in a body written in a file, saying how many uses
//! the script's every body has ([`ambient_uses`], once per crate).
//!
//! Not found: `println!("cargo::..")` (and `cargo:..`), which is how a script talks to the
//! build, not ambient authority; a type only named in a signature; an item only code a macro
//! wrote uses; `env!` and `option_env!`, which the compile reads. The lint looks at no crate
//! but a build script, and a dependency's build script is not linted at all (`blaze check`
//! lints the workspace's packages; a registry or git crate's lints are capped at `allow`). No
//! fix: which capability a script is handed, and by whom, is not a mechanical rewrite.

use base_db::{Db, TargetKind};
use blaze_diagnostics::lints::registry::ready;
use hir_def::{DefWithBodyId, expr_store::body::body_syntax};
use syntax::{NodeOrToken, TextRange};

use super::{
    Lint, frozen_collection,
    std_use::{self, Authority},
};
use crate::check::Cx;

const LINT: Lint = Lint::BuildScriptAmbient;

/// What every finding says of a script that reaches out by itself.
const AMBIENT: &str = "a build script that reaches for the file system, the environment or a \
                       process by itself cannot be analysed: blaze must run it in a sandbox, \
                       trace what it read, and rerun it whenever anything it might have read \
                       changes";

impl Authority {
    /// What a use of it does, as a finding's first line says it.
    const fn does(self) -> &'static str {
        match self {
            Authority::Fs => "reaches the file system",
            Authority::Env => "reads or changes the environment",
            Authority::Process => "starts or ends a process",
            Authority::Net => "reaches the network",
            Authority::Clock => "reads the clock",
        }
    }

    /// The capability of blaze's standard library that stands for it
    /// (docs/v2/plan/design/std.md).
    const fn capability(self) -> &'static str {
        match self {
            Authority::Fs => "`Fs` (a `Dir` of it for one directory)",
            Authority::Env => "`Env` (`Args` for the command line)",
            Authority::Process => "`Process` (`Runner`, to run a command in a sandbox)",
            Authority::Net => "`Net`",
            Authority::Clock => "`Clock`",
        }
    }
}

/// One item of std's ambient authority a build script uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Use {
    /// As a finding names it: `std::env::var`, `std::process::Command`.
    name: String,
    authority: Authority,
    /// How many paths and method calls of the script's bodies are of it.
    count: u32,
    /// The first of them in a body written in a file: the body and the use's range in its
    /// tree. `None` when only code a macro wrote uses it.
    first: Option<(DefWithBodyId, TextRange)>,
}

/// The items of std's ambient authority a crate's bodies use, in the order they are first met:
/// the crate's bodies in the order of its modules and their declarations.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Uses(Box<[Use]>);

/// Every use of std's ambient authority in `krate`'s bodies, as the module docs say, once per
/// crate; asked only of a build script.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn ambient_uses(db: &Db, krate: base_db::Crate) -> Uses {
    // alloc: the query's value, a few items, once per build script and edit
    let mut uses: Vec<Use> = Vec::new();
    for def in frozen_collection::crate_bodies(db, krate) {
        let written = body_syntax(db, def).is_some();
        let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
        // No row with an authority is a renamed type, the only rows that read the text.
        for used in std_use::uses_in(db, &analysis, &|_| "") {
            let Some(authority) = std_use::TABLE[used.row].authority else { continue };
            let at = match uses.iter().position(|it| it.name == used.name) {
                Some(at) => at,
                None => {
                    uses.push(Use { name: used.name, authority, count: 0, first: None });
                    uses.len() - 1
                }
            };
            let entry = &mut uses[at];
            entry.count += used.count;
            if entry.first.is_none()
                && written
                && let Some(range) = used.first
            {
                entry.first = Some((def, range));
            }
        }
    }
    Uses(uses.into_boxed_slice())
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let db = cx.db();
    if !matches!(cx.krate().target_kind(db), TargetKind::BuildScript) {
        return;
    }
    let Some(def) = cx.analysis().and_then(|it| DefWithBodyId::try_from(it.def()).ok()) else {
        return;
    };
    let uses = ambient_uses(db, cx.krate().base());
    for used in uses.0.iter() {
        let Some((first, range)) = used.first else { continue };
        if first != def {
            continue;
        }
        let root = cx.root();
        let at = match root.text_range().contains_range(range) {
            true => match root.covering_element(range) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => it.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(range);
        cx.blaze(LINT, at, span, message(&used.name, used.count, used.authority))
            .note(AMBIENT)
            .note(capability(&used.name, used.authority))
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The finding's first line: the item, how often the script uses it and what that does.
fn message(name: &str, count: u32, authority: Authority) -> String {
    let times = match count {
        1 => "once".to_owned(), // alloc: a message's part
        n => format!("{n} times"),
    };
    format!("`{name}`, used {times} in this build script, {} by itself", authority.does())
}

/// The note naming the capability that stands for `authority`: something to take now once
/// blaze's standard library is in the tree (`ready`), and what is planned until then.
fn capability(name: &str, authority: Authority) -> String {
    let capability = authority.capability();
    if ready::CAPABILITIES_FOR_BUILD_SCRIPTS {
        format!("take {capability} as a parameter in place of `{name}`")
    } else {
        format!(
            "the capability that replaces `{name}` is {capability}, of blaze's standard library, \
             which is planned and not in this blaze"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Authority, message};

    #[test]
    fn a_finding_names_the_item_its_count_and_what_it_does() {
        assert_eq!(
            message("std::env::var", 2, Authority::Env),
            "`std::env::var`, used 2 times in this build script, reads or changes the environment by itself"
        );
        assert_eq!(
            message("std::process::Command", 1, Authority::Process),
            "`std::process::Command`, used once in this build script, starts or ends a process by itself"
        );
    }
}
