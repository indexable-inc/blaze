//! `blocking_on_pool`: a call that blocks the OS thread, made by a function that runs on a fiber
//! or a pool thread (AGENTS.md rule 9: on a pool thread or fiber, a wait goes through the
//! scheduler, never a `Condvar`, a channel receive, `thread::sleep` or a raw `spin_loop`). A
//! blocked pool thread runs no other job until the call returns.
//!
//! Which functions run there is the may-suspend check's ([`hir_ty_suspend::on_fiber`]: what the
//! fiber driver's code reaches, and what may suspend), never a second analysis. A blocking call
//! is a `Condvar` wait (std's, `parking_lot`'s), a channel receive (std's `mpsc`,
//! `crossbeam_channel`'s), joining a thread, `thread::sleep` or `park`, `spin_loop`, or blocking
//! IO of `std::fs` and `std::net` (their functions, the methods of `File`, `OpenOptions`, the
//! sockets, and `Read` and `Write` on a file or a stream).

use base_db::Db;
use blaze_hash::HashSet;
use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _,
    hir::Expr,
    signatures::FunctionSignature,
};
use hir_ty::next_solver::GenericArgs;
use rustc_type_ir::inherent::GenericArgs as _;

use super::{Finding, Lint, alloc::written_callee, facts};
use crate::check::Cx;

/// The functions of a crate's walk that run on a fiber or a pool thread, as a set.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OnFiber(HashSet<FunctionId>);

impl OnFiber {
    pub(crate) fn contains(&self, f: FunctionId) -> bool {
        self.0.contains(&f)
    }
}

/// [`hir_ty_suspend::on_fiber`] of `krate`, once per crate, to ask of each body.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn on_fiber_set(db: &Db, krate: base_db::Crate) -> OnFiber {
    OnFiber(hir_ty_suspend::on_fiber(db, krate).iter().copied().collect())
}

/// What a finding says to wait with instead.
#[derive(Clone, Copy)]
enum Instead {
    Sleep,
    Event,
    Spin,
    Io,
}

impl Instead {
    fn say(self) -> &'static str {
        match self {
            Instead::Sleep => "wait for the deadline through the scheduler: `blaze_graph::sched::wait(Wait::Until(deadline))`",
            Instead::Event => {
                "wait through the scheduler: a `blaze_graph::sched::Event` the other side sets, or a `Wait::Key` or `Wait::Job` it completes"
            }
            Instead::Spin => {
                "wait through the scheduler (`blaze_graph::sched::wait`); off the pool, spin with `blaze_graph::spin`'s bounded spin-then-park"
            }
            Instead::Io => {
                "do the IO on a driver thread and wait for it through the scheduler (a `blaze_graph::sched::Event` the driver sets)"
            }
        }
    }
}

/// Where a type of [`METHODS`] is.
enum Of {
    /// At this path of `core`, `alloc` or `std`.
    Std(&'static [&'static str]),
    /// The type of this name in the crate of this name.
    Crate(&'static str, &'static str),
}

/// The blocking methods, by the type they are of, each by the start of its name.
const METHODS: &[(Of, &[&str], Instead)] = &[
    (Of::Std(&["std", "sync", "Condvar"]), &["wait"], Instead::Event),
    (Of::Crate("parking_lot", "Condvar"), &["wait"], Instead::Event),
    (Of::Std(&["std", "sync", "mpsc", "Receiver"]), &["recv", "iter", "into_iter"], Instead::Event),
    (Of::Crate("crossbeam_channel", "Receiver"), &["recv", "iter", "into_iter"], Instead::Event),
    (Of::Std(&["std", "thread", "JoinHandle"]), &["join"], Instead::Event),
    (
        Of::Std(&["std", "fs", "File"]),
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
            "try_lock",
            "unlock",
            "try_clone",
            "read",
            "write",
            "flush",
            "seek",
        ],
        Instead::Io,
    ),
    (Of::Std(&["std", "fs", "OpenOptions"]), &["open"], Instead::Io),
    (Of::Std(&["std", "net", "TcpStream"]), &["connect", "peek", "read", "write", "flush", "shutdown"], Instead::Io),
    (Of::Std(&["std", "net", "TcpListener"]), &["bind", "accept", "incoming"], Instead::Io),
    (Of::Std(&["std", "net", "UdpSocket"]), &["bind", "connect", "recv", "send", "peek"], Instead::Io),
];

/// The blocking free functions, by their path.
const FREE: &[(&[&str], Instead)] = &[
    (&["std", "thread", "sleep"], Instead::Sleep),
    (&["std", "thread", "park"], Instead::Event),
    (&["std", "thread", "park_timeout"], Instead::Event),
    (&["core", "hint", "spin_loop"], Instead::Spin),
    (&["std", "hint", "spin_loop"], Instead::Spin),
];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::BlockingOnPool) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(DefWithBodyId::FunctionId(owner)) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    if !on_fiber_set(db, owner.krate(db)).contains(owner) {
        return found;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    let owner_name = FunctionSignature::of(db, owner).name.as_str();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((f, args)) = written_callee(infer, body, id) else { continue };
        let Some((what, instead)) = blocking(cx, db, analysis, f, args) else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        let message = format!("{what} blocks the pool thread `{owner_name}` runs on");
        let note = format!(
            "`{owner_name}` runs on a fiber or a pool thread (the fiber driver reaches it, or it may suspend), and a \
             blocked pool thread runs no other job until the call returns (AGENTS.md rule 9): {}",
            instead.say()
        );
        facts::push(&mut found, Lint::BlockingOnPool, range, message, Some(note));
    }
    found
}

/// Whether a call of `f` with `args` blocks the thread: what it is, as a finding names it, and
/// what to wait with instead.
fn blocking<'db>(
    cx: &Cx<'_, '_>,
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    f: FunctionId,
    args: GenericArgs<'db>,
) -> Option<(String, Instead)> {
    let function = hir::Function::from(f);
    let name = FunctionSignature::of(db, f).name.as_str();
    let Some(item) = function.as_assoc_item(db) else {
        if let Some((path, instead)) =
            FREE.iter().find(|(path, _)| cx.std_def(path) == Some(hir::ModuleDef::Function(function)))
        {
            return Some((format!("`{}`", path.join("::")), *instead));
        }
        let fs = cx.std_def(&["std", "fs"])?;
        return (fs == hir::ModuleDef::Module(function.module(db))).then(|| (format!("`std::fs::{name}`"), Instead::Io));
    };
    let (self_ty, trait_) = match item.container(db) {
        hir::AssocItemContainer::Trait(it) => (analysis.ty(args.type_at(0)), Some(it)),
        hir::AssocItemContainer::Impl(it) => (it.self_ty(db), it.trait_(db)),
    };
    let adt = self_ty.strip_references().as_adt()?;
    let adt_name = adt.name(db);
    let row = METHODS.iter().find(|(of, prefixes, _)| {
        prefixes.iter().any(|it| name.starts_with(it))
            && match of {
                Of::Std(path) => cx.std_def(path) == Some(hir::ModuleDef::Adt(adt)),
                Of::Crate(krate, it) => adt_name.as_str() == *it && facts::crate_is(db, adt.module(db).krate(db), krate),
            }
    })?;
    let what = match trait_ {
        Some(trait_) => format!("`{}::{name}` on `{}`", trait_.name(db).as_str(), adt_name.as_str()),
        None => format!("`{}::{name}`", adt_name.as_str()),
    };
    Some((what, row.2))
}
