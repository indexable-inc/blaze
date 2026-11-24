//! The world's texts as a pool worker reads them: one home for body_types.rs's file jobs and
//! names.rs's crate scopes (rule 30). A worker shares no live world: it is handed the packs'
//! views and the corrections the command read (record bytes and a few words each), reads the
//! checkout's files itself, each once a job, into its own frame, and lists each it read or
//! looked for and did not find.

use core::cell::RefCell;
use std::alloc::Global;

use blaze_fetch::pack::PackView;
use blaze_scope::crate_env::CrateEnv;
use blaze_scope::resolve::{Found, SourceTexts, Unread};
use bstd::fs::FsKind;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::body_types::{LazyTrees, SharedTrees, SyncLazyTrees};
use crate::work::held_str;

/// Thread-safe subset of [`Packs`] for worker pools (`Open` or `SyncLazy`).
#[derive(Clone, Copy)]
pub(crate) enum SyncPacks<'n> {
    Open(&'n [PackView<'n>]),
    SyncLazy(&'n SyncLazyTrees<'n>, &'n bstd::fs::Fs),
}

impl<'n> From<SyncPacks<'n>> for Packs<'n> {
    fn from(packs: SyncPacks<'n>) -> Packs<'n> {
        match packs {
            SyncPacks::Open(views) => Packs::Open(views),
            SyncPacks::SyncLazy(lazy, fs) => Packs::SyncLazy(lazy, fs),
        }
    }
}

/// The packs of a crate list's trees as a job reads them, tree `k` (1-based; tree 0 is the
/// checkout) by [`Packs::tree`]: every tree opened before the job ([`Packs::Open`]: a check's
/// stages, whose pool jobs share one slice), or each opened on the first read of a text from it
/// ([`Packs::OnFirstRead`]: `blaze run`'s parent; [`Packs::SyncLazy`]: `Bodies` pool workers).
#[derive(Clone, Copy)]
pub(crate) enum Packs<'n> {
    Open(&'n [PackView<'n>]),
    OnFirstRead(&'n LazyTrees<'n>),
    Shared(SharedTrees<'n>),
    SyncLazy(&'n SyncLazyTrees<'n>, &'n bstd::fs::Fs),
}

impl<'n> Packs<'n> {
    /// Tree `k`'s pack; `None` past the list's trees, or (on first read) a tree that could not
    /// be opened, which `TreesSeen::refused` / `SyncLazyTrees::refused` (body_types.rs) names for
    /// the caller to stop on (rule 3).
    pub(crate) fn tree(&self, cx: &Cx, frame: &Frame, k: usize) -> Option<PackView<'n>> {
        match self {
            Packs::Open(views) => views.get(k.checked_sub(1)?).copied(),
            Packs::OnFirstRead(lazy) => lazy.tree(k),
            Packs::Shared(shared) => shared.tree(cx, frame, k),
            Packs::SyncLazy(lazy, fs) => lazy.tree(cx, fs, frame, k),
        }
    }
}

impl<'a: 'n, 'p: 'a, 'n> From<&'a [PackView<'p>]> for Packs<'n> {
    fn from(views: &'a [PackView<'p>]) -> Packs<'n> {
        Packs::Open(views)
    }
}

impl<'n> From<&'n LazyTrees<'n>> for Packs<'n> {
    fn from(lazy: &'n LazyTrees<'n>) -> Packs<'n> {
        Packs::OnFirstRead(lazy)
    }
}

impl<'n> From<SharedTrees<'n>> for Packs<'n> {
    fn from(shared: SharedTrees<'n>) -> Packs<'n> {
        Packs::Shared(shared)
    }
}

impl<'a: 'n, 'f: 'a, 'n> From<(&'a SyncLazyTrees<'f>, &'a bstd::fs::Fs)> for Packs<'n> {
    fn from((lazy, fs): (&'a SyncLazyTrees<'f>, &'a bstd::fs::Fs)) -> Packs<'n> {
        Packs::SyncLazy(lazy, fs)
    }
}

pub(crate) struct JobTexts<'n> {
    pub(crate) cx: &'n Cx,
    pub(crate) root: &'n Dir<ReadWrite>,
    pub(crate) frame: &'n Frame,
    pub(crate) packs: Packs<'n>,
    pub(crate) corrected: &'n [(u32, &'n str, &'n str)],
    /// The environment of the one crate the job is about, by its index among the job's crates
    /// (a names job's world index; a body job's owner, 0: body_types.rs `owner_env`): what
    /// `env!` reads. Any other crate's is `envs`' (a run's crates, run_names.rs `RunEnvs`); with
    /// neither, every `env!` asked through these texts is refused by name.
    pub(crate) env: Option<(usize, &'n CrateEnv<'n>)>,
    /// The environment of every OTHER crate of the job, each made at the first ask of it
    /// ([`EnvsByNeed`]: a run's crates, run_names.rs `RunEnvs`). `None`: only `env`'s crate has
    /// one, and any other crate's `env!` and include are refused by name (body_host.rs).
    pub(crate) envs: Option<&'n dyn EnvsByNeed<'n>>,
    /// Each checkout path asked (tree 0, after `/0/`), in the order first asked, with its text
    /// or `None` when there is no such file.
    pub(crate) read: RefCell<std::vec::Vec<(&'n str, Option<&'n str>), Global>>,
    /// Each checkout path [`SourceTexts::source`] answered `Unknown` (there, and not read as
    /// module text: not UTF-8, or the file system's error), with its cause, in the order asked.
    /// A read of the file all the same: the job's answer, a failure too, lists it as a read
    /// file (names/scope.rs), so the file's next content asks the job again.
    pub(crate) unread: RefCell<blaze_own::Vec<(&'n str, Unread<'n>), &'n Frame>>,
    /// Each checkout path read as DATA (`include_str!`, `include_bytes!`; tree 0, after `/0/`),
    /// with the checkout file whose call read it, in the order asked: no module, so never in
    /// `read` (a reader wants its bytes' digest, never its `Surface`: data_include.rs). Each
    /// with what the reader was handed ([`SourceTexts::read_data`]), `None` when it had nothing:
    /// the body digests it in the domain of the cell it reads (engine_bodies.rs `data_read`).
    pub(crate) data: RefCell<std::vec::Vec<(&'n str, &'n str, Option<&'n [u8]>), Global>>,
    /// Each include told to have left its own package inside the workspace (D111), in the order
    /// told: filed as warning findings at their calls (names/include_outside.rs).
    pub(crate) left: RefCell<std::vec::Vec<crate::names::include_outside::Left<'n>, Global>>,
    /// The command's proc-macro runner (proc_runner.rs), lent read-only, or its maker by need
    /// ([`Procs`]). [`Procs::None`]: no runner, so a proc-macro crate gets no key and a
    /// proc-macro call is refused by name where lowered.
    pub(crate) procs: Procs<'n>,
    /// The names job's one crate, by its world index: the crate whose runner key is asked.
    /// `None` for a body job (a body reads its dependencies' keys from their items records).
    pub(crate) own: Option<usize>,
    /// Each crate whose code this job may expand a proc-macro call in, by its `krate` index:
    /// its index in the NAMES world (the runner's key: one per unit, so a package's lib and
    /// bin are two, macro-env-per-unit) and its edition.
    pub(crate) callers: &'n [(usize, usize, blaze_expand::Edition)],
    /// The crates after the owner in a scope opened by need (run_names.rs `ScopeCrates::ByNeed`):
    /// crate `k > 0` of this job is `rest[k - 1]`, and it is a proc-macro caller too. A body job
    /// lowers a dependency's body here (front2 opaque_auto.rs checks another crate's defining
    /// body). The crate's edition is read from its records, opened at its first call and never
    /// at setup, which keeps piece C (a)'s floor. `None`: every caller is in `callers`.
    pub(crate) by_need: Option<(&'n [crate::front_load::Unopened], &'n dyn crate::front_load::OpenByNeed)>,
    /// The words of this job's last proc-macro run that stopped (its `CallError`), held in
    /// the frame: [`SourceTexts::proc_macro_unusable`] gives them for the call the solve asks
    /// about just after the run (one job, one thread: the run and the ask are in order).
    pub(crate) run_stopped: core::cell::Cell<Option<&'n str>>,
    /// Live status board and clock, when running inside `engine_check`: reports active
    /// Wasmtime proc-macro expansions (`WorkKind::ProcMacro`) while they execute.
    pub(crate) activity: Option<(&'n crate::engine_status::Board<'n>, &'n bstd::clock::Clock)>,
}

/// The environments of a job's crates past its one owner ([`JobTexts::envs`]), each made at
/// the first ask for it by the one construction the check's owner has too (body_types.rs
/// `row_env`, rule 30): Rust's `env!` reads "the environment of the crate being compiled"
/// (for a dependency, ITS package's `CARGO_PKG_*`, its `CARGO_MANIFEST_DIR`, its build
/// script's `cargo:rustc-env`), and its includes are "relative to the current file" of that
/// crate, so a body lowered by need is answered from its own crate, never from the job's owner.
pub(crate) trait EnvsByNeed<'n> {
    /// Crate `krate` of the job's environment; `None` when it could not be made (the reason
    /// kept by the maker for its own report) or there is no such crate: every `env!` and
    /// include of that crate is then refused by name (blaze-expand `HostError`).
    fn env_of(&self, krate: usize) -> Option<&'n CrateEnv<'n>>;
}

/// A job's proc-macro runner ([`JobTexts::procs`]): none, one the command made already (the
/// check's, made on a Names miss: engine_names.rs `runner`), or one made at the job's FIRST
/// proc-macro call ([`ProcsByNeed`]: `blaze run` and `blaze test`, engine_start.rs
/// `RunProcs`), so a command whose bodies call no proc macro never makes one. Whichever, it is
/// the one runner `proc_runner::make` makes (the sandboxed wasm host, rules 9 and 10), and its
/// answers are the records `check` keeps (rule 77).
#[derive(Clone, Copy)]
pub(crate) enum Procs<'n> {
    None,
    Made(&'n crate::proc_runner::ProcRunner<'static>),
    ByNeed(&'n dyn ProcsByNeed<'n>),
}

impl<'n> Procs<'n> {
    /// The form of a caller that holds a runner made already, or none.
    pub(crate) const fn of(made: Option<&'n crate::proc_runner::ProcRunner<'static>>) -> Procs<'n> {
        match made {
            Some(made) => Procs::Made(made),
            None => Procs::None,
        }
    }
}

/// The maker of a command's runner at its first ask ([`Procs::ByNeed`]): made once, the same
/// answer on every later ask, success or not ([`MadeOnce`]).
pub(crate) trait ProcsByNeed<'n> {
    /// The runner; `Err`: why none could be made, in the command's frame (rule 19).
    fn procs(&self) -> Result<&'static crate::proc_runner::ProcRunner<'static>, &'n str>;
}

/// What a maker by need answered: the runner, or why none could be made.
pub(crate) type MadeProcs<'n> = Result<&'static crate::proc_runner::ProcRunner<'static>, &'n str>;

/// THE [`ProcsByNeed`]: `make` run at the first ask and never again; every later ask, and the
/// command's end ([`MadeOnce::made`]: the runner's `keep`, or the refusal's one line), reads
/// what it answered.
pub(crate) struct MadeOnce<'n, F> {
    make: F,
    made: core::cell::OnceCell<MadeProcs<'n>>,
}

impl<'n, F: Fn() -> MadeProcs<'n>> MadeOnce<'n, F> {
    pub(crate) fn new(make: F) -> MadeOnce<'n, F> {
        MadeOnce { make, made: core::cell::OnceCell::new() }
    }

    /// What the first ask made; `None` when nothing asked (no proc-macro call was met).
    pub(crate) fn made(&self) -> Option<MadeProcs<'n>> {
        self.made.get().copied()
    }
}

impl<'n, F: Fn() -> MadeProcs<'n>> ProcsByNeed<'n> for MadeOnce<'n, F> {
    fn procs(&self) -> MadeProcs<'n> {
        *self.made.get_or_init(|| (self.make)())
    }
}

/// Why crate `krate` of a job is not a proc-macro caller of it ([`JobTexts::caller`]).
#[derive(Clone, Copy, Debug)]
enum NotCaller {
    /// Neither listed in `callers` nor in a scope opened by need.
    NotGiven { krate: usize },
    /// Past the crates of the scope opened by need.
    PastScope { krate: usize, crates: usize },
    /// Its records did not open: the store's words.
    DidNotOpen { krate: usize, name: &'static str, why: &'static str },
}

impl core::fmt::Display for NotCaller {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NotCaller::NotGiven { krate } => write!(f, "the calling crate (index {krate} of this job) is not one this job was given as a proc-macro caller"),
            NotCaller::PastScope { krate, crates } => write!(f, "the calling crate (index {krate} of this job) is past the {crates} crates of its scope"),
            NotCaller::DidNotOpen { krate, name, why } => write!(f, "the calling crate `{name}` (index {krate} of this job): its records did not open, so its edition is unknown: {why}"),
        }
    }
}

impl<'n> JobTexts<'n> {
    /// This job's runner: `Ok(None)` when it has none, the maker's words when a runner by need
    /// could not be made. A runner by need is made here, at the first ask.
    fn runner(&self) -> Result<Option<&'n crate::proc_runner::ProcRunner<'static>>, &'n str> {
        match self.procs {
            Procs::None => Ok(None),
            Procs::Made(made) => Ok(Some(made)),
            Procs::ByNeed(maker) => maker.procs().map(Some),
        }
    }

    /// Crate `krate` of this job as a proc-macro caller: its index in the names world and its
    /// edition. It is either in `callers` or a crate of the scope opened by need (`by_need`);
    /// in the second case its records are opened here, once per crate (front_load.rs
    /// `ScopeRecords`: the same borrow for every later ask).
    fn caller(&self, krate: usize) -> Result<(usize, blaze_expand::Edition), NotCaller> {
        if let Some(&(_, world_at, edition)) = self.callers.iter().find(|(at, _, _)| *at == krate) {
            return Ok((world_at, edition));
        }
        let Some((rest, records)) = self.by_need else { return Err(NotCaller::NotGiven { krate }) };
        let Some(one) = krate.checked_sub(1).and_then(|k| rest.get(k)) else { return Err(NotCaller::PastScope { krate, crates: rest.len() + 1 }) };
        match records.open(one) {
            Ok(opened) => Ok((one.world_at, opened.edition)),
            Err(why) => Err(NotCaller::DidNotOpen { krate, name: one.name, why }),
        }
    }

    fn checkout(&self, inner: &str) -> Option<&'n str> {
        core::str::from_utf8(self.checkout_bytes(inner)?).ok()
    }

    /// The checkout file `inner` as [`SourceTexts::source`] answers: its bytes, `Absent` when no
    /// file is there (a path with `.` or `..`, one no `Path` names, not found, a directory),
    /// `Unknown` with the file system's error for any other failure (a permission, a read that
    /// moved, the driver), which [`JobTexts::checkout_bytes`] reads as absent. The bytes and
    /// the error are held in the job's frame (`self.frame`, `'n`: freed with the job), as the
    /// seam's lifetime asks.
    fn checkout_found(&self, inner: &str) -> Found<&'n [u8], Unread<'n>> {
        if inner.split('/').any(|part| part == ".." || part == ".") {
            return Found::Absent;
        }
        let Ok(at) = Path::new(inner) else { return Found::Absent };
        let mut bytes = Bytes::new_in(self.frame, "a pool job's checkout files");
        if let Err(error) = self.root.read_all(self.cx, &at, &mut bytes) {
            return match error.kind {
                FsKind::NotFound | FsKind::IsADirectory | FsKind::NotADirectory => Found::Absent,
                _ => {
                    let why: &'n bstd::fs::FsError = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(error, self.frame));
                    Found::Unknown(Unread { path: held_str(inner, self.frame), why })
                }
            };
        }
        let held: &'n Bytes<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(bytes, self.frame));
        Found::Known(held.as_slice())
    }

    fn checkout_bytes(&self, inner: &str) -> Option<&'n [u8]> {
        if inner.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        let at = Path::new(inner).ok()?;
        let mut bytes = Bytes::new_in(self.frame, "a pool job's checkout files");
        self.root.read_all(self.cx, &at, &mut bytes).ok()?;
        let held: &'n Bytes<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(bytes, self.frame));
        Some(held.as_slice())
    }
}

impl<'n> SourceTexts<'n> for JobTexts<'n> {
    fn read(&self, _krate: usize, path: &str) -> Option<&'n str> {
        let rest = path.strip_prefix('/')?;
        let (tree, inner) = rest.split_once('/')?;
        let tree: usize = tree.parse().ok()?;
        if let Some((_, _, text)) = self.corrected.iter().find(|(t, p, _)| *t as usize == tree && *p == inner) {
            return Some(text);
        }
        if tree != 0 {
            return core::str::from_utf8(self.packs.tree(self.cx, self.frame, tree)?.find(inner)?.bytes).ok();
        }
        if let Some((_, text)) = self.read.borrow().iter().find(|(p, _)| *p == inner) {
            return *text;
        }
        let text = self.checkout(inner);
        self.read.borrow_mut().push((held_str(inner, self.frame), text));
        text
    }

    /// [`SourceTexts::read`]'s answer with an unread checkout file kept apart from an absent
    /// one: `Unknown` names the file and the error, and is not listed in `read` (a listed
    /// absence would be kept as a fact of the crate; an unread file is none). A file that is
    /// not UTF-8 is `Unknown` with its UTF-8 error: it is there, and is no module text.
    fn source(&self, krate: usize, path: &str) -> Found<&'n str, Unread<'n>> {
        let Some(rest) = path.strip_prefix('/') else { return Found::Absent };
        let Some((tree, inner)) = rest.split_once('/') else { return Found::Absent };
        let Ok(tree) = tree.parse::<usize>() else { return Found::Absent };
        if tree != 0 || self.corrected.iter().any(|(t, p, _)| *t as usize == tree && *p == inner) {
            // A pack's file or a corrected text: tables, read in memory.
            return Found::of_table(self.read(krate, path));
        }
        if let Some((_, text)) = self.read.borrow().iter().find(|(p, _)| *p == inner) {
            return Found::of_table(*text);
        }
        if let Some((_, unread)) = self.unread.borrow().iter().find(|(p, _)| *p == inner) {
            return Found::Unknown(*unread);
        }
        let unknown = |unread: Unread<'n>| {
            self.unread.borrow_mut().push((unread.path, unread));
            Found::Unknown(unread)
        };
        let text = match self.checkout_found(inner) {
            Found::Known(bytes) => match core::str::from_utf8(bytes) {
                Ok(text) => Some(text),
                Err(error) => {
                    let why: &'n core::str::Utf8Error = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(error, self.frame));
                    return unknown(Unread { path: held_str(inner, self.frame), why });
                }
            },
            Found::Absent => None,
            Found::Unknown(unread) => return unknown(unread),
        };
        self.read.borrow_mut().push((held_str(inner, self.frame), text));
        Found::of_table(text)
    }

    /// A file read as data: its bytes (any bytes: `include_bytes!` of a binary file), from the
    /// pack or the checkout as [`SourceTexts::read`] reads a text. A checkout file is listed in
    /// `data` with the calling file `from`, found or not (a missing one is read absent, so
    /// creating it asks the reader again). `from` is in the same tree: blaze-scope's
    /// `include_place` keeps a member's include inside the workspace (the checkout, D111) and a
    /// dependency's inside its package.
    fn read_data(&self, _krate: usize, from: &str, path: &str) -> Option<&'n [u8]> {
        let rest = path.strip_prefix('/')?;
        let (tree, inner) = rest.split_once('/')?;
        let tree: usize = tree.parse().ok()?;
        if tree != 0 {
            return Some(self.packs.tree(self.cx, self.frame, tree)?.find(inner)?.bytes);
        }
        let bytes = self.checkout_bytes(inner);
        let from = from.strip_prefix("/0/").unwrap_or(from);
        // What the reader was handed, the one read (rule 77: never read twice), so a body's row
        // holds the digest of what it read whichever cell defines the file.
        self.data.borrow_mut().push((held_str(inner, self.frame), held_str(from, self.frame), bytes));
        bytes
    }

    /// A read of a call (an include, a member's proc macro) that left its package inside the
    /// workspace (D111): kept, to be filed as a warning finding at its call by the crate's names
    /// record.
    fn left_package(&self, _krate: usize, left: blaze_scope::include_place::LeftPackage<'_>) {
        let from = held_str(left.from, self.frame);
        let path = held_str(left.path, self.frame);
        // The macro as written at the call (`macros::sol`), no longer a builtin's static name.
        let call = held_str(left.call, self.frame);
        self.left.borrow_mut().push((from, left.at, left.shape, call, path));
    }

    /// The owner's environment (`env`), else crate `krate`'s made by need (`envs`).
    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'n>> {
        match self.env {
            Some((at, env)) if at == krate => Some(env),
            _ => self.envs?.env_of(krate),
        }
    }

    /// The runner key of the names job's crate when it is a proc-macro crate: its module
    /// identity's key (proc_runner.rs). It does not depend on whether the store has the
    /// module, so a missing module is refused at each call and named by the `MacroModule` task,
    /// never `NoDefinition` (pm-call-expand).
    fn proc_macro_key(&self, krate: usize) -> Option<blaze_id::CrateItemKey> {
        if self.own != Some(krate) {
            return None;
        }
        // A maker's refusal gives no key here; each call of the crate is then refused with the
        // maker's words ([`Self::run_proc_macro`], [`Self::proc_macro_unusable`]).
        self.runner().ok().flatten()?.key_of(krate)
    }

    /// A proc-macro call written in the crate `krate`: through the runner's one path
    /// (`ProcMacros::run`), shown the crate's own package at its edition. No runner, or a crate
    /// this job was not given as a caller, is refused with the expander's typed `HostError`.
    fn run_proc_macro<'b>(
        &self,
        krate: usize,
        def: &blaze_expand::MacroDefinition<'_>,
        call: blaze_expand::ProcCall<'_>,
        alloc: blaze_own::Alloc<'b>,
    ) -> Result<blaze_expand::Produced<'b>, blaze_expand::HostError> {
        let refused = blaze_expand::HostError { ask: blaze_expand::HostAsk::ProcMacro, why: blaze_expand::HostWhy::Refused };
        // Each run starts with no stopped run kept: what `proc_macro_unusable` names is this one's.
        self.run_stopped.set(None);
        let procs = match self.runner() {
            Ok(Some(procs)) => procs,
            Ok(None) => return Err(refused),
            // The runner could not be made: its words kept for the ask after this run, and the
            // call failed by its runner (`Failed`), never `Refused` as by a job given none.
            Err(words) => {
                self.run_stopped.set(Some(words));
                return Err(blaze_expand::HostError { ask: blaze_expand::HostAsk::ProcMacro, why: blaze_expand::HostWhy::Failed });
            }
        };
        let Ok((world_at, edition)) = self.caller(krate) else { return Err(refused) };
        let _active = self.activity.map(|(board, clock)| {
            let name = procs.crate_name(def).or_else(|| def.proc_name()).unwrap_or("proc-macro");
            board.show_activity(
                blaze_screen::WorkKind::ProcMacro,
                crate::engine_status::Label::of(name),
                board.now(clock),
            )
        });
        procs.run(self.cx, self.packs, world_at, edition, def, call, alloc).map_err(|stopped| {
            if let Some(error) = &stopped.call {
                self.run_stopped.set(Some(held_str(&std::format!("{error}"), self.frame)));
            }
            stopped.host
        })
    }

    /// Why a refused run of `def` was refused, by the same steps [`Self::run_proc_macro`]
    /// takes: no runner in this job, a crate this job was not given as a caller, the runner's
    /// reason (proc_runner.rs `unusable`: the macro's crate and its module), or, the module able
    /// to run it, the words of the run that stopped (a sandbox limit naming the path as the
    /// macro named it, a variable, a failure: blaze-proc-macro `CallError`). Kept in the job's
    /// frame: the names record's failed-call row holds these words.
    fn proc_macro_unusable(&self, krate: usize, def: &blaze_expand::MacroDefinition<'_>) -> Option<blaze_scope::resolve::ProcUnusable<'n>> {
        use blaze_scope::resolve::ProcUnusable;
        let words = match self.runner() {
            Ok(None) => std::string::String::from("this names job has no proc-macro runner (the command made none: proc_runner.rs `make`)"),
            // Held in the frame by the maker already.
            Err(words) => return Some(ProcUnusable::Module(words)),
            Ok(Some(procs)) => match self.caller(krate) {
                Err(not) => std::format!("{not}"),
                Ok((world_at, _)) => match procs.unusable(self.cx, self.packs, world_at, def) {
                    Some(words) => words,
                    None => return self.run_stopped.take().map(ProcUnusable::Run),
                },
            },
        };
        Some(ProcUnusable::Module(held_str(&words, self.frame)))
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    /// A maker's planted refusal: what the job's refusals must name.
    const WORDS: &str = "the proc-macro runner: planted refusal (job_texts.rs tests)";

    /// A names job of the crate 0 (so its runner key is asked) with no files and `procs`.
    fn texts<'n>(cx: &'n Cx, root: &'n Dir<ReadWrite>, procs: Procs<'n>) -> JobTexts<'n> {
        JobTexts {
            cx,
            root,
            frame: cx.frame(),
            packs: Packs::Open(&[]),
            corrected: &[],
            env: None,
            envs: None,
            read: RefCell::new(std::vec::Vec::new()),
            unread: RefCell::new(blaze_own::Vec::new_in(cx.frame())),
            data: RefCell::new(std::vec::Vec::new()),
            left: RefCell::new(std::vec::Vec::new()),
            procs,
            own: Some(0),
            callers: &[],
            by_need: None,
            run_stopped: core::cell::Cell::new(None),
            activity: None,
        }
    }

    /// C9 (wave-1119; `blaze run` and `blaze test` had no runner, so every proc-macro call a
    /// run's body met was refused): a runner by need is made at the FIRST ask and never at setup
    /// or for a crate that asks nothing; it is made once, every later ask reading that answer; a
    /// maker's refusal is named by its words (rule 19), never a bare refusal. Control: a job given
    /// no runner (`Procs::None`, what engine_start.rs passed before) has none and asks no maker.
    #[test]
    fn a_runner_by_need_is_made_at_the_first_ask_once_and_its_refusal_is_named() {
        let io = Io::simulated(SimWorld::new(91, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
        let asked = Cell::new(0u32);
        let maker = MadeOnce::new(|| {
            asked.set(asked.get() + 1);
            Err(WORDS)
        });
        let job = texts(&cx, &root, Procs::ByNeed(&maker));
        assert_eq!(asked.get(), 0, "nothing is made at setup");
        assert!(job.proc_macro_key(1).is_none(), "crate 1 is not the job's own: no key asked");
        assert_eq!(asked.get(), 0, "a crate whose key is not asked makes no runner");
        assert!(job.proc_macro_key(0).is_none(), "the maker refused: no key");
        assert_eq!(asked.get(), 1, "the first ask makes the runner");
        assert_eq!(job.runner().err(), Some(WORDS), "the maker's words are the job's refusal (rule 19)");
        assert_eq!(asked.get(), 1, "made once: a later ask reads what the first made");
        assert_eq!(maker.made().and_then(|made| made.err()), Some(WORDS), "the command's end reads the same answer");

        let none = texts(&cx, &root, Procs::None);
        assert!(matches!(none.runner(), Ok(None)), "control: a job given no runner has none");
        assert!(none.proc_macro_key(0).is_none(), "control: no runner, no key");
        assert_eq!(asked.get(), 1, "control: a job given no runner asks no maker");
    }
}
