//! `blocking_in_async`: a call that blocks the OS thread, written inside an `async fn`, an
//! `async` block or an `async` closure. An executor polls many tasks on few threads; while the
//! call blocks, no other task on that thread runs, and a single-threaded runtime stops.
//!
//! A blocking call is, by the callee resolved (never by the text written): a free function of
//! `std::fs` (`read`, `write`, `metadata`, ..), the methods of `fs::File` and
//! `OpenOptions::open`; `thread::sleep`; the blocking methods of `std::net`'s `TcpStream`,
//! `TcpListener`, `UdpSocket` and `Incoming` (`connect`, `accept`, `read`, `write`, `recv`,
//! `send`, `next`), `Read` and `Write` on them included; `Stdin`'s reads; a receive on std's
//! `mpsc::Receiver` or crossbeam-channel's `Receiver`, and a send on std's bounded
//! `SyncSender`; `process::Command::{output, status}` and `Child::wait`; and
//! `thread::JoinHandle::join`.
//!
//! A closure inside the `async` body ends the walk up: it runs when called, maybe on another
//! thread (`spawn_blocking(move || std::fs::read(p))` is the fix, not a finding). A call that is
//! awaited is not one of these.
//!
//! Narrowed: `std::sync::Mutex::lock` and `RwLock::{read, write}` are not counted. A short
//! critical section under a std lock is what async runtimes recommend over an async lock, and
//! the case that does block the executor, a guard held across an `.await`, is clippy's
//! `await_holding_lock`, so counting the lock here would warn on good code or fire twice. A send
//! on crossbeam-channel's `Sender` is not counted either: the type is the same for `bounded` and
//! `unbounded`, and only a bounded one blocks.

use base_db::Db;
use hir_def::{DefWithBodyId, hir::Expr};

use super::{
    Finding, Lint, facts,
    general_facts::{self, Callee},
};
use crate::check::Cx;

const LINT: Lint = Lint::BlockingInAsync;

/// What a finding says to use instead.
#[derive(Clone, Copy)]
enum Instead {
    Fs,
    Sleep,
    Net,
    Channel,
    Process,
    Join,
}

impl Instead {
    fn say(self) -> &'static str {
        match self {
            Instead::Fs => {
                "use the runtime's file API (`tokio::fs`, `async_std::fs`) or move the call into `spawn_blocking`"
            }
            Instead::Sleep => "await the runtime's timer (`tokio::time::sleep`, `async_io::Timer`)",
            Instead::Net => {
                "use the runtime's sockets (`tokio::net`, `async_std::net`) or move the call into `spawn_blocking`"
            }
            Instead::Channel => {
                "use an async channel (`tokio::sync::mpsc`, `async-channel`, flume's `recv_async`) or move the wait into `spawn_blocking`"
            }
            Instead::Process => {
                "use the runtime's process API (`tokio::process::Command`) or move the call into `spawn_blocking`"
            }
            Instead::Join => {
                "await a task's handle (`tokio::task::JoinHandle`) or join inside `spawn_blocking`"
            }
        }
    }
}

/// Where a type of [`METHODS`] is.
enum Of {
    /// In this module (the first under the crate root) of core, alloc or std.
    Std(&'static str),
    /// In the crate of this name.
    Crate(&'static str),
}

/// The blocking methods: the type they are of, by where it is and its names, and the starts of
/// the methods' names.
const METHODS: &[(Of, &[&str], &[&str], Instead)] = &[
    (
        Of::Std("fs"),
        &["File"],
        &[
            "open",
            "create",
            "sync_",
            "set_len",
            "metadata",
            "set_permissions",
            "set_times",
            "set_modified",
            "lock",
            "read",
            "write",
            "flush",
            "seek",
        ],
        Instead::Fs,
    ),
    (Of::Std("fs"), &["OpenOptions"], &["open"], Instead::Fs),
    (
        Of::Std("net"),
        &["TcpStream"],
        &["connect", "peek", "read", "write", "flush", "shutdown"],
        Instead::Net,
    ),
    (Of::Std("net"), &["TcpListener"], &["bind", "accept"], Instead::Net),
    (Of::Std("net"), &["Incoming"], &["next"], Instead::Net),
    (Of::Std("net"), &["UdpSocket"], &["bind", "connect", "recv", "send", "peek"], Instead::Net),
    (Of::Std("io"), &["Stdin", "StdinLock"], &["read", "lines"], Instead::Fs),
    (Of::Std("sync"), &["Receiver"], &["recv", "iter"], Instead::Channel),
    (Of::Std("sync"), &["SyncSender"], &["send"], Instead::Channel),
    (Of::Crate("crossbeam_channel"), &["Receiver"], &["recv", "iter"], Instead::Channel),
    (Of::Std("process"), &["Command"], &["output", "status"], Instead::Process),
    (Of::Std("process"), &["Child"], &["wait"], Instead::Process),
    (Of::Std("thread"), &["JoinHandle"], &["join"], Instead::Join),
];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
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
    if DefWithBodyId::try_from(analysis.def()).is_err() {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some(callee) = general_facts::callee(db, analysis, id) else { continue };
        let Some((what, instead)) = blocking(db, &callee) else { continue };
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        if parents.of_expr(id).is_some_and(|it| matches!(body.expr(it), Expr::Await { .. })) {
            continue;
        }
        let Some(context) = general_facts::async_context(body, parents, id) else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        let message = format!(
            "{what} blocks the thread inside this {}: the executor runs no other task on the thread until it returns",
            context.say()
        );
        // alloc: a finding's note
        facts::push(&mut found, LINT, range, message, Some(instead.say().to_owned()));
    }
    found
}

/// Whether calling `callee` blocks the thread: what it is, as a message names it, and what to
/// use instead.
fn blocking(db: &Db, callee: &Callee<'_>) -> Option<(String, Instead)> {
    let Some(adt) = callee.self_adt else {
        if callee.is_std_free(db, "fs", &[]) {
            return Some((format!("`std::fs::{}`", callee.name), Instead::Fs));
        }
        if callee.is_std_free(db, "thread", &["sleep"]) {
            return Some(("`std::thread::sleep`".to_owned(), Instead::Sleep)); // alloc: a message's part
        }
        return None;
    };
    let (_, _, _, instead) = METHODS.iter().find(|(of, types, prefixes, _)| {
        prefixes.iter().any(|it| callee.name.starts_with(it))
            && match of {
                Of::Std(top) => general_facts::std_adt(db, adt, top, types),
                Of::Crate(krate) => general_facts::crate_adt(db, adt, krate, types),
            }
    })?;
    let what = match callee.trait_ {
        Some(trait_) => format!(
            "`{}::{}` on `{}`",
            trait_.name(db).as_str(),
            callee.name,
            adt.name(db).as_str()
        ),
        None => format!("`{}::{}`", adt.name(db).as_str(), callee.name),
    };
    Some((what, *instead))
}
