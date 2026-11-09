//! `sans_io_core`: a function of a sans-IO core that reaches an ambient effect through some
//! call chain (AGENTS.md rule 14, sans-io-cores: a core is a synchronous state machine that
//! reads time only from an injected clock, owns no socket, file, thread, timer or sleep, and
//! hands back decisions and deadlines).
//!
//! A core is a module, or a crate, marked with the inner attribute `#![blaze::sans_io]` (a
//! blaze extension: `AttrFlags::SANS_IO`, inert under rustc with `#![feature(register_tool,
//! custom_inner_attributes)]` and `#![register_tool(blaze)]` at the crate root); its children
//! are part of it. Code under `#[cfg(test)]` or `#[test]` is not checked: a test drives the
//! core from outside.
//!
//! Each call of a core function's body is walked with `hot_alloc`'s walk
//! ([`alloc::reaches_root`]), its root set [`Roots::Ambient`] ([`ambient`]): the wall clock
//! (`Instant::now`, `SystemTime::now`), `std::thread`, `std::fs`, `std::net`, `std::process`,
//! `std::env`, `std::io::{stdin, stdout, stderr}`, OS randomness (`RandomState::new`,
//! `getrandom`, `rand::thread_rng`, `OsRng`), tokio and mio, channel timeouts
//! (`crossbeam_channel`'s `recv_timeout` and `after`, std's `mpsc` `recv_timeout`), and libc
//! (and `rustix`, `nix`). The walk goes through the workspace's own functions, since a helper
//! outside the core that reads the clock is the core reading it; a callee that is itself in a
//! core is not a finding at the call, its own body has it. A call through a trait object or a
//! type parameter the core takes resolves to no body and reaches nothing: that is how a driver
//! injects IO. std's locks and panic machinery (`std::sync`, `std::panicking`, `std::rt`,
//! `std::alloc`), `parking_lot`, `lock_api`, `tracing`, `log` and `blaze_mem` end the walk not
//! reaching: they are not effects a core chooses.
//!
//! The message names the core function, the chain from it to the root, the root, and the rule.

use base_db::Db;
use hir::AsAssocItem as _;
use hir_def::{
    AttrDefId, DefWithBodyId, FunctionId, HasModule as _, attrs::AttrFlags, hir::Expr,
    signatures::FunctionSignature,
};
use hir_ty::{ParamEnvAndCrate, db::HirDatabase as _};

use super::{
    Finding, Lint,
    alloc::{self, Resolved, Roots},
    facts,
};
use crate::check::Cx;

/// The most steps a finding's chain names: past it the chain ends in `…` (the walk itself is
/// bounded by its memo; this bounds only the naming, rule 18).
const LONGEST_CHAIN: usize = 24;

/// The rule every message ends with.
const RULE: &str = "a core reads time only from an injected clock and does no IO";

/// What a function is to the ambient walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Ambient {
    /// Walked into: its calls decide.
    Walk,
    /// Not followed: it reaches nothing a core chooses (a lock, the panic machinery, tracing).
    Stop,
    /// An ambient effect itself.
    Root(Effect),
}

impl Ambient {
    pub(super) fn is_root(self) -> bool {
        matches!(self, Ambient::Root(_))
    }

    pub(super) fn stops(self) -> bool {
        matches!(self, Ambient::Stop)
    }
}

/// Which ambient effect a root is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Effect {
    Clock,
    Thread,
    Fs,
    Net,
    Process,
    Env,
    Stdio,
    Random,
    Async,
    ChannelTimeout,
    Syscall,
}

impl Effect {
    fn say(self) -> &'static str {
        match self {
            Effect::Clock => "the wall clock",
            Effect::Thread => "a thread",
            Effect::Fs => "the file system",
            Effect::Net => "the network",
            Effect::Process => "a process",
            Effect::Env => "the environment",
            Effect::Stdio => "stdio",
            Effect::Random => "OS randomness",
            Effect::Async => "an async runtime",
            Effect::ChannelTimeout => "a channel timeout (the wall clock)",
            Effect::Syscall => "a syscall",
        }
    }
}

/// What `f` is to the ambient walk, by its crate, its module path and its name: once per
/// function.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn ambient(db: &Db, f: FunctionId) -> Ambient {
    let function = hir::Function::from(f);
    let module = function.module(db);
    let krate = module.krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    let name = FunctionSignature::of(db, f).name.as_str();
    let self_adt = facts::impl_adt(db, f).map(|it| it.name(db));
    let self_is = |names: &[&str]| self_adt.as_ref().is_some_and(|it| names.contains(&it.as_str()));
    if is("std") {
        // alloc: once per function of std the walk meets, its module path
        let path: Vec<hir::Name> =
            module.path_to_root(db).into_iter().rev().filter_map(|it| it.name(db)).collect();
        let segment = |at: usize| path.get(at).map(hir::Name::as_str);
        return match segment(0) {
            Some("time") if name == "now" && self_is(&["Instant", "SystemTime"]) => {
                Ambient::Root(Effect::Clock)
            }
            Some("thread") => Ambient::Root(Effect::Thread),
            Some("fs") => Ambient::Root(Effect::Fs),
            Some("net") => Ambient::Root(Effect::Net),
            Some("process") => Ambient::Root(Effect::Process),
            Some("env") => Ambient::Root(Effect::Env),
            Some("io")
                if segment(1) == Some("stdio") && matches!(name, "stdin" | "stdout" | "stderr") =>
            {
                Ambient::Root(Effect::Stdio)
            }
            Some("hash") if name == "new" && self_is(&["RandomState"]) => {
                Ambient::Root(Effect::Random)
            }
            Some("sync") if matches!(name, "recv_timeout" | "recv_deadline" | "send_timeout") => {
                Ambient::Root(Effect::ChannelTimeout)
            }
            Some("sync" | "panicking" | "rt" | "alloc") => Ambient::Stop,
            _ => Ambient::Walk,
        };
    }
    if is("getrandom") {
        return Ambient::Root(Effect::Random);
    }
    if (is("rand") || is("rand_core"))
        && (matches!(name, "thread_rng" | "rng" | "random") || self_is(&["OsRng", "ThreadRng"]))
    {
        return Ambient::Root(Effect::Random);
    }
    if is("tokio") || is("mio") {
        return Ambient::Root(Effect::Async);
    }
    if is("crossbeam_channel")
        && matches!(
            name,
            "recv_timeout"
                | "recv_deadline"
                | "send_timeout"
                | "send_deadline"
                | "after"
                | "at"
                | "tick"
                | "select_timeout"
                | "ready_timeout"
        )
    {
        return Ambient::Root(Effect::ChannelTimeout);
    }
    if is("libc") || is("rustix") || is("nix") {
        return Ambient::Root(Effect::Syscall);
    }
    if [
        "parking_lot",
        "parking_lot_core",
        "lock_api",
        "tracing",
        "tracing_core",
        "log",
        "blaze_mem",
    ]
    .into_iter()
    .any(is)
    {
        return Ambient::Stop;
    }
    Ambient::Walk
}

/// The core `f` is in: its module or the nearest ancestor marked `#![blaze::sans_io]`.
pub(super) fn core_of(db: &Db, f: FunctionId) -> Option<hir::Module> {
    let mut module = hir::Function::from(f).module(db);
    loop {
        if AttrFlags::query(db, AttrDefId::ModuleId(module.into())).contains(AttrFlags::SANS_IO) {
            return Some(module);
        }
        module = module.parent(db)?;
    }
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::SansIoCore) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let DefWithBodyId::FunctionId(owner) = def else { return found };
    let db = cx.db();
    let Some(core) = core_of(db, owner) else { return found };
    if cx.is_in_test(cx.root()) {
        return found;
    }
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(def.generic_def(db)),
        krate: def.krate(db),
    };
    let body = analysis.body();
    let infer = analysis.infer();
    let owner_name = fn_name(db, owner);
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((callee, args)) = alloc::written_callee(infer, body, id) else { continue };
        let resolved = alloc::resolve(db, env, callee, args);
        // A core's own function is checked in its own body; a call of it is none.
        if let Resolved::Function(f, _) = resolved
            && !Roots::Ambient.is_root(db, f)
            && core_of(db, f).is_some()
        {
            continue;
        }
        if !alloc::instance_reaches_root(db, env, resolved, true, Roots::Ambient) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let walked = chain(db, env, resolved);
        let effect = walked.effect.map_or("an ambient effect", Effect::say);
        let message = format!(
            "`{owner_name}` reaches {effect} through `{root}`: `{owner_name}` → {steps}; `{owner_name}` is in the \
             sans-IO core `{core}`, and {RULE}",
            root = walked.root,
            steps = walked.steps,
            core = module_path(db, core),
        );
        let note = "AGENTS.md rule 14 (sans-io-cores): a core owns no socket, file, thread, timer or sleep; \
                    a driver does the IO and hands the core what it read";
        facts::push(&mut found, Lint::SansIoCore, range, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}

/// A finding's chain, from the call to the root.
struct Chain {
    /// The steps, each named, joined by `→`.
    steps: String,
    /// The root's full path, or the last step named when the chain was cut.
    root: String,
    effect: Option<Effect>,
}

/// The chain from `first`, what a core function's call runs, to the ambient root it reaches,
/// at most [`LONGEST_CHAIN`] steps.
fn chain<'db>(db: &'db Db, env: ParamEnvAndCrate<'db>, first: Resolved<'db>) -> Chain {
    // alloc: a finding's chain, reported at the edge
    let mut names: Vec<String> = Vec::new();
    let mut at = first;
    let mut cut = true;
    for _ in 0..LONGEST_CHAIN {
        match alloc::next_step(db, env, at, true, Roots::Ambient) {
            Some(next) => {
                names.push(step_name(db, at));
                at = next;
            }
            None => {
                cut = false;
                break;
            }
        }
    }
    let (root, effect) = match at {
        Resolved::Function(f, _) => match ambient(db, f) {
            Ambient::Root(effect) => (root_path(db, f), Some(effect)),
            Ambient::Walk | Ambient::Stop => (step_name(db, at), None),
        },
        Resolved::Derived(..) => (step_name(db, at), None),
    };
    // clone: the root's name, once in the chain and once on its own
    names.push(root.clone());
    if cut {
        names.push("…".to_owned()); // alloc: a finding's chain
    }
    let steps = names.iter().map(|it| format!("`{it}`")).collect::<Vec<_>>().join(" → "); // alloc: a finding's chain
    Chain { steps, root, effect }
}

/// How a chain names a step: `Type::method`, or a free function's name.
fn step_name(db: &Db, step: Resolved<'_>) -> String {
    match step {
        Resolved::Function(f, _) => fn_name(db, f),
        Resolved::Derived(impl_, method, _) => match method.trait_method(db, impl_) {
            Some(f) => format!("{} (derived)", fn_name(db, f)),
            None => "a derived method".to_owned(), // alloc: a finding's chain
        },
    }
}

/// `f` as `Type::name` (its impl's type, or its trait), or its name for a free function.
pub(super) fn fn_name(db: &Db, f: FunctionId) -> String {
    let name = FunctionSignature::of(db, f).name.as_str();
    if let Some(adt) = facts::impl_adt(db, f) {
        return format!("{}::{name}", adt.name(db).as_str());
    }
    match hir::Function::from(f).as_assoc_item(db).map(|it| it.container(db)) {
        Some(hir::AssocItemContainer::Trait(it)) => format!("{}::{name}", it.name(db).as_str()),
        _ => name.to_owned(), // alloc: a finding's chain
    }
}

/// `f`'s full path, its crate and modules first: how a finding names the root.
pub(super) fn root_path(db: &Db, f: FunctionId) -> String {
    format!("{}::{}", module_path(db, hir::Function::from(f).module(db)), fn_name(db, f))
}

/// `module`'s path, its crate's name first.
fn module_path(db: &Db, module: hir::Module) -> String {
    let krate =
        module.krate(db).display_name(db).map(|it| it.crate_name().symbol().as_str().to_owned());
    // alloc: a finding's path, reported at the edge
    let mut path: Vec<String> = krate.into_iter().collect();
    path.extend(
        module
            .path_to_root(db)
            .into_iter()
            .rev()
            .filter_map(|it| it.name(db))
            .map(|it| it.as_str().to_owned()),
    );
    path.join("::")
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const STD: &str = r#"
//- minicore: sized, copy, clone, fn, option
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod time {
    pub struct Instant(u64);
    impl Instant {
        pub fn now() -> Instant {
            Instant(0)
        }
        pub fn elapsed(&self) -> u64 {
            Instant::now().0 - self.0
        }
    }
}
pub mod fs {
    pub fn read(_path: &str) -> u64 {
        0
    }
}
pub mod thread {
    pub fn park() {}
}
pub mod sync {
    pub struct Mutex(u64);
    impl Mutex {
        pub fn lock(&self) -> u64 {
            crate::thread::park();
            self.0
        }
    }
}
//- /src/lib.rs crate:main deps:std
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
"#;

    fn lint_main(main: &str) -> String {
        lint(
            &format!("{STD}{}", main.trim_start_matches('\n')),
            "-A clippy::all --warn=blaze::sans_io_core",
        )
    }

    /// A core's direct read of the clock and its read of a file through a helper outside the
    /// core are found, each naming its chain and the rule; the clock read through a type
    /// parameter or a trait object, a lock, and the same reads outside the core are not.
    #[test]
    fn a_core_reaching_the_clock_or_a_file_is_found() {
        let found = lint_main(
            r#"
pub mod machine {
    #![blaze::sans_io]
    pub trait Clock {
        fn now(&self) -> u64;
    }
    pub struct Machine {
        at: u64,
    }
    impl Machine {
        pub fn advance_clock(&mut self, now: u64) {
            self.at = now;
        }
        pub fn injected<C: Clock>(&mut self, clock: &C, other: &dyn Clock) {
            self.at = clock.now() + other.now();
        }
        pub fn direct(&mut self) {
            let _ = std::time::Instant::now();
        }
        pub fn through_helper(&mut self) {
            self.at = crate::helper();
        }
        pub fn elapsed(&mut self, since: &std::time::Instant) {
            self.at = since.elapsed();
        }
        pub fn calls_its_core(&mut self) {
            self.direct();
        }
        pub fn locks(&mut self, m: &std::sync::Mutex) {
            self.at = m.lock();
        }
    }
}
pub fn helper() -> u64 {
    std::fs::read("x")
}
pub fn driver() {
    let _ = std::time::Instant::now();
}
"#,
        );
        assert_eq!(found.matches("does no IO").count(), 3, "{found}");
        assert!(
            found.contains(
                "`Machine::direct` reaches the wall clock through `std::time::Instant::now`"
            ),
            "{found}"
        );
        assert!(
            found.contains("`Machine::through_helper` → `helper` → `std::fs::read`"),
            "a chain through the workspace names each step:\n{found}"
        );
        assert!(
            found.contains("`Machine::elapsed` → `Instant::elapsed` → `std::time::Instant::now`"),
            "{found}"
        );
        assert!(found.contains("is in the sans-IO core `main::machine`"), "{found}");
        assert!(
            !found.contains("`Machine::injected`"),
            "IO injected through a parameter is fine:\n{found}"
        );
        assert!(
            !found.contains("`Machine::calls_its_core`"),
            "a core callee is found in its own body:\n{found}"
        );
        assert!(!found.contains("`Machine::locks`"), "std's locks end the walk:\n{found}");
        assert!(!found.contains("`driver`"), "a driver outside the core reads the clock:\n{found}");
    }

    /// The marker on the crate root makes every module a core; a test inside it is not checked.
    #[test]
    fn a_crate_marked_as_a_core_covers_its_modules_but_not_its_tests() {
        let found = lint_main(
            r#"
#![blaze::sans_io]
pub mod inner {
    pub fn reads() -> u64 {
        std::fs::read("x")
    }
}
#[cfg(test)]
mod tests {
    fn reads_in_a_test() -> u64 {
        std::fs::read("y")
    }
}
"#,
        );
        assert_eq!(found.matches("does no IO").count(), 1, "{found}");
        assert!(
            found.contains("`reads` reaches the file system through `std::fs::read`"),
            "{found}"
        );
        assert!(found.contains("the sans-IO core `main`"), "{found}");
    }
}
