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
//! - the clock: `Instant::now`, `SystemTime::now`.
//!
//! Such a script cannot be analysed: blaze must run it in a sandbox, trace what it read, and
//! rerun it whenever anything it might have read changes. A script that is handed a capability
//! (docs/v2/plan/design/std.md: `Fs`, `Env`, `Process`, `Net`, `Clock`) states in its signature
//! what it can touch.
//!
//! A use is a path that names the function, called or not (`std::fs::read(p)`,
//! `.map(std::fs::read)`, `File::open(p)`), found by the function the path's type is, as
//! inference resolved it: through a `use`, an alias, a glob or a re-export, never by the text
//! written. One finding per distinct item per script (a free function, or the type for a
//! type's functions), at its first use in a body written in a file, saying how many uses the
//! script's every body has ([`ambient_uses`], once per crate). A method call on a value
//! (`file.set_len(0)`, `cmd.status()`) is not counted: the value came from a path that is.
//!
//! Not found: `println!("cargo::..")` (and `cargo:..`), which is how a script talks to the
//! build, not ambient authority; a type only named in a signature; an item only code a macro
//! wrote uses; `env!` and `option_env!`, which the compile reads. The lint looks at no crate
//! but a build script, and a dependency's build script is not linted at all (`blaze check`
//! lints the workspace's packages; a registry or git crate's lints are capped at `allow`). No
//! fix: which capability a script is handed, and by whom, is not a mechanical rewrite.

use base_db::{Db, TargetKind};
use blaze_diagnostics::lints::registry::ready;
use hir::AsAssocItem as _;
use hir_def::{
    CallableDefId, DefWithBodyId, FunctionId, expr_store::body::body_syntax, hir::Expr,
};
use hir_ty::next_solver::TyKind;
use rustc_type_ir::inherent::IntoKind as _;
use syntax::{NodeOrToken, TextRange};

use super::{Lint, facts, frozen_collection};
use crate::check::Cx;

const LINT: Lint = Lint::BuildScriptAmbient;

/// What every finding says of a script that reaches out by itself.
const AMBIENT: &str = "a build script that reaches for the file system, the environment or a \
                       process by itself cannot be analysed: blaze must run it in a sandbox, \
                       trace what it read, and rerun it whenever anything it might have read \
                       changes";

/// `std::env`'s functions that read or change the process's environment, working directory or
/// command line; `split_paths` and `join_paths` only compute.
const ENV: &[&str] = &[
    "var",
    "var_os",
    "vars",
    "vars_os",
    "set_var",
    "remove_var",
    "current_dir",
    "set_current_dir",
    "args",
    "args_os",
    "current_exe",
    "temp_dir",
    "home_dir",
];

/// `std::fs`'s types whose functions reach the file system; `Metadata`, `Permissions` and
/// `FileType` are data already read.
const FS_TYPES: &[&str] = &["File", "OpenOptions", "DirBuilder", "ReadDir", "DirEntry"];

const PROCESS_FNS: &[&str] = &["exit", "abort", "id"];
const PROCESS_TYPES: &[&str] = &["Command", "Child"];

/// `std::net`'s types that own a socket; an address is data.
const NET_TYPES: &[&str] = &["TcpStream", "TcpListener", "UdpSocket"];

const CLOCK_TYPES: &[&str] = &["Instant", "SystemTime"];

/// Which ambient authority an item of std is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Authority {
    Fs,
    Env,
    Process,
    Net,
    Clock,
}

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
    /// How many paths of the script's bodies name it.
    count: u32,
    /// The first of them in a body written in a file: the body and the path's range in its
    /// tree. `None` when only code a macro wrote uses it.
    first: Option<(DefWithBodyId, TextRange)>,
}

/// The items of std's ambient authority a crate's bodies use, in the order they are first met:
/// the crate's bodies in the order of its modules and their declarations.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Uses(Box<[Use]>);

/// What stands for one finding: a free function, or the type whose functions are used.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Function(FunctionId),
    Adt(hir::Adt),
}

/// Every use of std's ambient authority in `krate`'s bodies, as the module docs say, once per
/// crate; asked only of a build script.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn ambient_uses(db: &Db, krate: base_db::Crate) -> Uses {
    // alloc: the query's value and its keys, a few items, once per build script and edit
    let mut keys: Vec<Key> = Vec::new();
    let mut uses: Vec<Use> = Vec::new();
    for def in frozen_collection::crate_bodies(db, krate) {
        let written = body_syntax(db, def).is_some();
        let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
        let body = analysis.body();
        let infer = analysis.infer();
        for (id, expr) in body.exprs() {
            if !matches!(expr, Expr::Path(_)) {
                continue;
            }
            let TyKind::FnDef(callable, _) = infer.expr_ty(id).kind() else { continue };
            let CallableDefId::FunctionId(f) = callable.0 else { continue };
            let Some((key, authority, name)) = classify(db, f) else { continue };
            let at = match keys.iter().position(|it| *it == key) {
                Some(at) => at,
                None => {
                    keys.push(key);
                    uses.push(Use { name, authority, count: 0, first: None });
                    uses.len() - 1
                }
            };
            let entry = &mut uses[at];
            entry.count += 1;
            if !written {
                continue;
            }
            let Some(range) = analysis.expr_range(id) else { continue };
            match &mut entry.first {
                // The expressions of a body are not met in the order they are written.
                Some((first_def, first)) if *first_def == def => {
                    if range.start() < first.start() {
                        *first = range;
                    }
                }
                Some(_) => {}
                None => entry.first = Some((def, range)),
            }
        }
    }
    Uses(uses.into_boxed_slice())
}

/// Which ambient authority the function `f` is, what its finding is keyed by and how it is
/// named: by the crate it is defined in (`std`), the first module under that crate's root
/// (`fs` for `std::fs::File::open`, `net` for `std::net::tcp`'s `TcpStream`) and, for a type's
/// function, the type its inherent impl is of.
fn classify(db: &Db, f: FunctionId) -> Option<(Key, Authority, String)> {
    let func = hir::Function::from(f);
    let module = func.module(db);
    if !facts::crate_is(db, module.krate(db), "std") {
        return None;
    }
    let top = module.path_to_root(db).into_iter().rev().find_map(|it| it.name(db))?;
    let top = top.as_str();
    let name = func.name(db);
    let name = name.as_str();
    // A trait's method (`Read::read`, `From::from`), its own or an impl's, is not the type's:
    // only an inherent impl's function is.
    let adt = match func.as_assoc_item(db).map(|it| it.container(db)) {
        None => None,
        Some(hir::AssocItemContainer::Trait(_)) => return None,
        Some(hir::AssocItemContainer::Impl(it)) => {
            if it.trait_(db).is_some() {
                return None;
            }
            Some(it.self_ty(db).strip_references().as_adt()?)
        }
    };
    let Some(adt) = adt else {
        let authority = match top {
            "fs" => Authority::Fs,
            "env" if ENV.contains(&name) => Authority::Env,
            "process" if PROCESS_FNS.contains(&name) => Authority::Process,
            _ => return None,
        };
        return Some((Key::Function(f), authority, format!("std::{top}::{name}")));
    };
    let ty = adt.name(db);
    let ty = ty.as_str();
    let authority = match top {
        "fs" if FS_TYPES.contains(&ty) => Authority::Fs,
        "process" if PROCESS_TYPES.contains(&ty) => Authority::Process,
        "net" if NET_TYPES.contains(&ty) => Authority::Net,
        // Only reading the clock is ambient: `duration_since` and the rest compute.
        "time" if CLOCK_TYPES.contains(&ty) && name == "now" => {
            return Some((Key::Function(f), Authority::Clock, format!("std::time::{ty}::now")));
        }
        _ => return None,
    };
    Some((Key::Adt(adt), authority, format!("std::{top}::{ty}")))
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
