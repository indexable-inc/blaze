//! `#[bstd::test]` with grants (D109): a test names what it takes in its signature, as `main`
//! does (D104), and each parameter after `cx` is made for it by its TYPE, through the same
//! [`Grant`] trait, [`Given`] and [`launch_body!`](crate::launch_body) the launcher fills
//! `main` with (one mechanism, rule 30).
//!
//! ```ignore
//! #[bstd::test(data = files("ledger/head" => b"0\n"))]
//! fn the_ledger_keeps_what_it_committed(cx: &Cx, data: Dir<ReadWrite>, clock: Clock) -> Result<(), FsError> { .. }
//! ```
//!
//! - **The world a parameter comes from.** Each seed's world is [`Io::sim`]'s, made by
//!   [`boot`], and it is handed to [`Given`] as the process's world; the run spec [`spec_of`]
//!   builds from the manifest grants each parameter that does not fill itself: a `Dir` named
//!   `data` is the folder `data` beneath the simulated root and nothing above it (a grant of
//!   `dir:data`), a `Net` is the simulated host's, narrowed to the endpoint the attribute names,
//!   an `Env` reads only the variables the attribute sets. `Clock`, `Out`, `Args`, `Mem`, `Spec`
//!   and `In` fill themselves from the simulated world; what `out` and `err` wrote is in the
//!   failure report.
//! - **What a grant starts with is said in the attribute** ([`Setup`]), never drawn from the
//!   machine: a folder's files inline (`data = files("a/b" => b"..", "c" => include_bytes!(..))`:
//!   a `macro_rules!` attribute cannot list a directory, and each `include_bytes!` names the one
//!   file it reads, so the key of the test sees every read), the environment (`env(LANG = "C")`),
//!   the command line (`args("--fast")`), standard input (`input = b"..\n"`). Defaults: an empty
//!   folder, an empty environment, no arguments, an input that has ended. A fixture DIRECTORY
//!   (every file under `fixtures/ledger`, unnamed) is a build-time read that needs the
//!   language's help: a macro that lists a directory reads the machine, and that listing is a
//!   read of the test's key, so it waits for blaze's own build-time read with its key. Nobody
//!   adds a run-time read of the machine to get it.
//! - **The faults a seed explores are those that can reach a parameter** ([`reach`], a `const
//!   fn` over the kinds): a test with no `Dir<ReadWrite>` meets no crash, one with nothing but
//!   `cx`, `Out`, `Args` or `Env` meets no fault at all and is run ONCE, as the pure function
//!   of its code it is. `without(X)` naming a fault no parameter can meet is a compile error.
//! - **A test on the machine says so**: `#[bstd::test(real)]` gets its grants beneath a
//!   directory made for it (`bstd::test::native`), is run once, and its failure says it is not
//!   replayable by seed.
//!
//! Every argument is checked as the crate compiles ([`Setup::checked`], [`Planned::of`]): a
//! grant argument naming no parameter of its kind, a `Net` with no endpoint, an argument given
//! twice, `real` with a seed: each a compile error that says what to write.

use core::panic::AssertUnwindSafe;

use blaze_core::{
    FileName,
    sim::{
        Sim,
        seeds::{self, Ended, Failed, Fault, Given as SeedsGiven, Name, Replay, Run, Seeds, World},
    },
};
use blaze_own::{String, Vec};
use blaze_text::Writes as _;

use super::{BUDGET, Outcome, listed};
use crate::{
    grant::{Given, Kind, LaunchAlloc, Manifest, Param, Problem, Refused, Spec, Started, Want, same},
    host::Exit,
    io::{Io, Limits, SimWorld},
    mem::TASK_FRAME,
    rt::catch_unwind,
    spec::{GrantName, Granted, RunSpec, Scope},
    task::said,
};

/// The most folders and the most endpoints one test's attribute may set up.
pub const MOST_GRANTS: usize = 8;

/// One file a folder's grant starts with: its path beneath the folder and its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct File {
    pub path: &'static str,
    pub bytes: &'static [u8],
}

impl File {
    /// The file `path` holding `bytes`; a path that is empty, absolute or has an empty, `.` or
    /// `..` part is a compile error.
    pub const fn new(path: &'static str, bytes: &'static [u8]) -> File {
        if !relative(path) {
            panic!("bstd::test: a fixture's path is relative, without empty, `.` or `..` parts: `files(\"a/b\" => ..)`");
        }
        File { path, bytes }
    }
}

/// What the folder parameter `grant` starts with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Files {
    pub grant: &'static str,
    pub files: &'static [File],
}

/// The endpoint of the network parameter `grant`: connected to, or listened on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Endpoint {
    pub grant: &'static str,
    pub at: &'static str,
    pub listen: bool,
}

/// One environment variable of the test's world.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Var {
    pub name: &'static str,
    pub value: &'static str,
}

impl Var {
    pub const fn new(name: &'static str, value: &'static str) -> Var {
        Var { name, value }
    }
}

const NO_FILES: Files = Files { grant: "", files: &[] };
const NO_ENDPOINT: Endpoint = Endpoint { grant: "", at: "", listen: false };

/// The attribute's arguments, as a value made as the crate compiles: what is not a
/// capability (the seed, the fault kinds left out, the machine) and what each grant starts
/// with. Built by `__test_setup!` from the attribute, one `const fn` an argument, each
/// refusing a second of itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Setup {
    /// `seed = N`: this seed alone.
    pub seed: Option<u64>,
    /// `without(..)`: the fault kinds left out by name, a bit each ([`bit`]).
    pub left_out: u8,
    /// `real`: the machine, not the simulation.
    pub real: bool,
    dirs: [Files; MOST_GRANTS],
    dirs_len: usize,
    nets: [Endpoint; MOST_GRANTS],
    nets_len: usize,
    pub env: Option<&'static [Var]>,
    pub args: Option<&'static [&'static str]>,
    pub input: Option<&'static [u8]>,
    /// `cases = N`: cases per world seed of a test with drawn parameters (drawn.rs `CASES`).
    pub cases: Option<u32>,
    /// `kept`: the crate's `sim-kept.tapes` text and its package name, replayed first.
    pub kept: Option<(&'static str, &'static str)>,
}

/// A fault kind's bit.
pub const fn bit(fault: Fault) -> u8 {
    1 << fault as u8
}

/// Every fault kind's bit.
pub const ALL_FAULTS: u8 = bit(Fault::IoFailures) | bit(Fault::Delays) | bit(Fault::Crashes);

/// The fault kinds a parameter of `kind` can meet, a bit each: what fills itself from inside
/// the process (`Args`, `Out`, `Mem`, `Spec`, `Heap`, `Env`) meets none; a clock meets late
/// completions; a folder read only and an endpoint meet failed effects and late ones; a folder
/// that writes, a whole capability and any kind not named here meet every kind, a power cut
/// that tears unsynced writes too. (`Kind::In`, standard input, falls in the last class until
/// it is named beside `Net`: a follow-up once P7's `Kind::In` is in.)
pub const fn reach(kind: Kind) -> u8 {
    if matches!(kind, Kind::Args | Kind::Out | Kind::Mem | Kind::Spec | Kind::Heap | Kind::Env) {
        0
    } else if matches!(kind, Kind::Clock) {
        bit(Fault::Delays)
    } else if matches!(kind, Kind::Dir { write: false } | Kind::Net) {
        bit(Fault::IoFailures) | bit(Fault::Delays)
    } else {
        ALL_FAULTS
    }
}

impl Setup {
    /// No argument: every default.
    pub const DEFAULT: Setup = Setup {
        seed: None,
        left_out: 0,
        real: false,
        dirs: [NO_FILES; MOST_GRANTS],
        dirs_len: 0,
        nets: [NO_ENDPOINT; MOST_GRANTS],
        nets_len: 0,
        env: None,
        args: None,
        input: None,
        cases: None,
        kept: None,
    };

    #[must_use]
    pub const fn seed(mut self, seed: u64) -> Setup {
        if self.seed.is_some() {
            panic!("bstd::test: `seed = ..` is given twice");
        }
        self.seed = Some(seed);
        self
    }

    #[must_use]
    pub const fn without(mut self, fault: Fault) -> Setup {
        if self.left_out & bit(fault) != 0 {
            panic!("bstd::test: `without(..)` names one fault kind twice");
        }
        self.left_out |= bit(fault);
        self
    }

    /// `cases = N`: 1 to 4096 cases per world seed (each a whole world run).
    #[must_use]
    pub const fn cases(mut self, n: u32) -> Setup {
        if self.cases.is_some() {
            panic!("bstd::test: `cases = ..` is given twice");
        }
        if n == 0 || n > 4096 {
            panic!("bstd::test: `cases = N` is 1 to 4096: each case is a whole world run");
        }
        self.cases = Some(n);
        self
    }

    /// `kept`: the crate's `sim-kept.tapes` (its text, compiled in) replayed first.
    #[must_use]
    pub const fn kept(mut self, text: &'static str, krate: &'static str) -> Setup {
        if self.kept.is_some() {
            panic!("bstd::test: `kept` is given twice");
        }
        self.kept = Some((text, krate));
        self
    }

    #[must_use]
    pub const fn real(mut self) -> Setup {
        if self.real {
            panic!("bstd::test: `real` is given twice");
        }
        self.real = true;
        self
    }

    #[must_use]
    pub const fn files(mut self, grant: &'static str, files: &'static [File]) -> Setup {
        let mut at = 0;
        while at < self.dirs_len {
            if same(self.dirs[at].grant, grant) {
                panic!("bstd::test: `<folder> = files(..)` is given twice for one parameter");
            }
            at += 1;
        }
        let mut a = 0;
        while a < files.len() {
            let mut b = a + 1;
            while b < files.len() {
                if same(files[a].path, files[b].path) {
                    panic!("bstd::test: `files(..)` names one path twice");
                }
                b += 1;
            }
            a += 1;
        }
        if self.dirs_len == MOST_GRANTS {
            panic!("bstd::test: more folders set up than MOST_GRANTS (8)");
        }
        self.dirs[self.dirs_len] = Files { grant, files };
        self.dirs_len += 1;
        self
    }

    /// The network parameter `grant` connects to (`listen: false`) or listens on `at`, a
    /// `host:port` checked here ([`crate::net::check_literal`]).
    #[must_use]
    pub const fn net(mut self, grant: &'static str, at: &'static str, listen: bool) -> Setup {
        crate::net::check_literal(at);
        let mut i = 0;
        while i < self.nets_len {
            if same(self.nets[i].grant, grant) {
                panic!("bstd::test: `<net> = connect(..)` or `listen(..)` is given twice for one parameter");
            }
            i += 1;
        }
        if self.nets_len == MOST_GRANTS {
            panic!("bstd::test: more endpoints set up than MOST_GRANTS (8)");
        }
        self.nets[self.nets_len] = Endpoint { grant, at, listen };
        self.nets_len += 1;
        self
    }

    #[must_use]
    pub const fn env(mut self, vars: &'static [Var]) -> Setup {
        if self.env.is_some() {
            panic!("bstd::test: `env(..)` is given twice");
        }
        let mut a = 0;
        while a < vars.len() {
            let mut b = a + 1;
            while b < vars.len() {
                if same(vars[a].name, vars[b].name) {
                    panic!("bstd::test: `env(..)` sets one variable twice");
                }
                b += 1;
            }
            a += 1;
        }
        self.env = Some(vars);
        self
    }

    #[must_use]
    pub const fn args(mut self, args: &'static [&'static str]) -> Setup {
        if self.args.is_some() {
            panic!("bstd::test: `args(..)` is given twice");
        }
        self.args = Some(args);
        self
    }

    #[must_use]
    pub const fn input(mut self, bytes: &'static [u8]) -> Setup {
        if self.input.is_some() {
            panic!("bstd::test: `input = ..` is given twice");
        }
        self.input = Some(bytes);
        self
    }

    /// The folders set up.
    pub fn dirs(&self) -> &[Files] {
        &self.dirs[..self.dirs_len]
    }

    /// The endpoints set up.
    pub fn nets(&self) -> &[Endpoint] {
        &self.nets[..self.nets_len]
    }

    /// The endpoint of the parameter `grant`.
    pub fn endpoint(&self, grant: &str) -> Option<&Endpoint> {
        self.nets().iter().find(|it| it.grant == grant)
    }

    /// The attribute checked against the test's parameters `params`, as the crate compiles:
    /// every grant argument names a parameter of its kind, every `Net` has its endpoint, what
    /// sets up the environment, the command line or the input has a parameter that reads it,
    /// no parameter is a whole `Io`, and `real` comes with nothing that only a simulated world
    /// takes.
    pub const fn checked(self, params: &[Param]) -> Setup {
        let mut i = 0;
        while i < params.len() {
            let param = params[i];
            if same(param.kind.name(), "everything") {
                panic!(
                    "bstd::test: a test cannot take a whole `Io` (D109): name what it uses after `cx`, \
                     e.g. `data: Dir<ReadWrite>, clock: Clock, out: Out, api: Net`"
                );
            }
            if matches!(param.kind, Kind::Net) && !self.has_net(param.name) {
                panic!(
                    "bstd::test: a `Net` parameter needs its endpoint in the attribute: \
                     `<name> = connect(\"host:port\")` or `<name> = listen(\"host:port\")`"
                );
            }
            i += 1;
        }
        let mut d = 0;
        while d < self.dirs_len {
            if !has_kind(params, self.dirs[d].grant, KindClass::Dir) {
                panic!("bstd::test: `<name> = files(..)` names no `Dir<ReadOnly>` or `Dir<ReadWrite>` parameter of the test");
            }
            d += 1;
        }
        let mut n = 0;
        while n < self.nets_len {
            if !has_kind(params, self.nets[n].grant, KindClass::Net) {
                panic!("bstd::test: `<name> = connect(..)` or `listen(..)` names no `Net` parameter of the test");
            }
            n += 1;
        }
        if self.env.is_some() && !has_class(params, KindClass::Env) {
            panic!("bstd::test: `env(..)` is given and the test takes no `Env`: add `env: Env` or remove it");
        }
        if self.args.is_some() && !has_class(params, KindClass::Args) {
            panic!("bstd::test: `args(..)` is given and the test takes no `Args`: add `args: Args` or remove it");
        }
        if self.input.is_some() && !has_class(params, KindClass::In) {
            panic!("bstd::test: `input = ..` is given and the test takes no `In`: add `input: In` or remove it");
        }
        if self.real && (self.seed.is_some() || self.left_out != 0) {
            panic!("bstd::test: `real` is the machine: it has no seed and no fault kinds to leave out; remove `seed`/`without`");
        }
        if self.real && (self.env.is_some() || self.args.is_some() || self.input.is_some()) {
            panic!(
                "bstd::test: `real` gives an empty environment, command line and input \
                 (bstd::test::native); `env`, `args` and `input` are the simulated world's"
            );
        }
        self
    }

    const fn has_net(&self, grant: &str) -> bool {
        let mut i = 0;
        while i < self.nets_len {
            if same(self.nets[i].grant, grant) {
                return true;
            }
            i += 1;
        }
        false
    }
}

/// How a test is run, decided as the crate compiles from its [`Setup`] and its parameters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Planned {
    /// The fault kinds its seeds inject: those its parameters can meet, less those left out.
    pub faults: u8,
    /// Run once: no fault can reach it (a pure function of its code), or it is `real`.
    pub once: bool,
    /// Its seeds, when it is not run once.
    pub seeds: Seeds,
    /// How many parameters are drawn from a tape (simtest/param.rs `drawn`); 0: none, and the
    /// test is run by the grants' runner alone.
    pub drawn: u32,
}

impl Planned {
    /// The plan of a test of `params` set up by `setup` (the module's doc). `without(X)` for
    /// an `X` no parameter can meet is refused; so is a `seed` on a test that is run once.
    pub const fn of(setup: &Setup, params: &[Param]) -> Planned {
        let mut reached = 0;
        let mut i = 0;
        while i < params.len() {
            reached |= reach(params[i].kind);
            i += 1;
        }
        if setup.left_out & bit(Fault::IoFailures) & !reached != 0 {
            panic!("bstd::test: `without(IoFailures)`: no parameter of this test can meet a failed effect; remove it");
        }
        if setup.left_out & bit(Fault::Delays) & !reached != 0 {
            panic!("bstd::test: `without(Delays)`: no parameter of this test can meet a late completion; remove it");
        }
        if setup.left_out & bit(Fault::Crashes) & !reached != 0 {
            panic!("bstd::test: `without(Crashes)`: no parameter of this test writes to a folder a power cut can tear; remove it");
        }
        let faults = reached & !setup.left_out;
        let once = faults == 0 || setup.real;
        if once && setup.seed.is_some() {
            panic!("bstd::test: `seed = ..` on a test no fault can reach: it is run once, with no seed; remove it");
        }
        let mut seeds = Seeds::DEFAULT;
        if let Some(seed) = setup.seed {
            seeds = seeds.from(seed, 1);
        }
        let mut f = 0;
        while f < Fault::ALL.len() {
            if faults & bit(Fault::ALL[f]) == 0 {
                seeds = seeds.without(Fault::ALL[f]);
            }
            f += 1;
        }
        Planned { faults, once, seeds, drawn: 0 }
    }

    /// The plan of a test with `drawn` drawn parameters: its cases are drawn from a tape per
    /// world seed (simtest/drawn.rs), so it is not run once even when no fault reaches it (it
    /// then runs its cases under ONE world seed). `cases` and `kept` on a test that draws
    /// nothing, and `real` with drawn values (replayed by seed and tape, which the machine is
    /// not), are refused.
    pub const fn drawing(mut self, setup: &Setup, drawn: u32) -> Planned {
        if drawn == 0 {
            if setup.cases.is_some() {
                panic!("bstd::test: `cases = N` on a test with no drawn parameter: it has no cases; remove it");
            }
            if setup.kept.is_some() {
                panic!("bstd::test: `kept` on a test with no drawn parameter: it keeps no tape; remove it");
            }
            return self;
        }
        if setup.real {
            panic!("bstd::test: `real` with drawn parameters: a drawn case is replayed by seed and tape, which the machine is not");
        }
        self.drawn = drawn;
        self.once = false;
        self
    }
}

/// One test, as its attribute expanded it: its name, its manifest, its setup and its plan.
#[derive(Clone, Copy)]
pub struct Test {
    /// Its package (`CARGO_PKG_NAME`), its module (`module_path!()`) and its function's name.
    pub package: &'static str,
    pub module: &'static str,
    pub name: &'static str,
    /// Its parameters after `cx`: data in the test binary, as a `main`'s manifest is
    /// ([`Manifest::write`] gives its JSON): with the setup, what a test's record is keyed by
    /// besides its code (test.md, `TestInputs`' run spec digest).
    pub manifest: &'static Manifest,
    pub setup: &'static Setup,
    pub planned: Planned,
}

/// Why a test's world or spec could not be made: each names the parameter or the file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SetupError {
    /// A parameter's name is no grant's name (checked by `Param::new` as the crate compiled).
    GrantName { param: &'static str },
    /// A fixture's path is no name beneath the folder.
    FixturePath { grant: &'static str, path: &'static str },
    /// An `env(..)` variable's name is not one.
    VarName { name: &'static str },
    /// A `real` test's directory could not be made, or its world opened: what was said.
    Real { said: String<LaunchAlloc> },
    /// A `real` test in a crate whose bstd is built without the `testing` feature.
    RealNeedsTesting,
}

impl SetupError {
    pub(super) fn line(&self, out: &mut String<LaunchAlloc>) {
        match self {
            SetupError::GrantName { param } => {
                out.str("the parameter `").str(param).str("` is no grant's name");
            }
            SetupError::FixturePath { grant, path } => {
                out.str("the fixture `").str(path).str("` of `").str(grant).str("` is no path beneath it");
            }
            SetupError::VarName { name } => {
                out.str("`env(").str(name).str(" = ..)`: no variable's name");
            }
            SetupError::Real { said } => {
                out.str("a real test's directory: ").str(said.as_str());
            }
            SetupError::RealNeedsTesting => {
                out.str("`real` needs bstd's `testing` feature in the crate's dev-dependencies");
            }
        }
    }
}

/// The run spec of a test: one grant per parameter that does not fill itself, each narrowed
/// to what the attribute set up. `alloc` is the test entry's heap (D102).
pub fn spec_of(test: &Test, alloc: LaunchAlloc) -> Result<Spec, SetupError> {
    let mut spec = RunSpec::defaults(Vec::new_in(alloc), Vec::new_in(alloc), String::from_str_in("/", alloc), alloc);
    for param in test.manifest.params {
        let scope = match param.kind {
            Kind::Dir { write } => Scope::Dir { path: String::from_str_in(param.name, alloc), write },
            Kind::Net => match test.setup.endpoint(param.name) {
                Some(endpoint) => Scope::Net { at: String::from_str_in(endpoint.at, alloc), listen: endpoint.listen },
                None => unreachable!("Setup::checked refused a `Net` with no endpoint as the crate compiled"),
            },
            Kind::Env => {
                let vars = test.setup.env.unwrap_or(&[]);
                let mut names = Vec::with_capacity_in(vars.len(), alloc);
                for var in vars {
                    if crate::env::VarName::new(var.name).is_err() {
                        return Err(SetupError::VarName { name: var.name });
                    }
                    names.push(String::from_str_in(var.name, alloc));
                }
                names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
                Scope::Env { names }
            }
            Kind::Whole(capability) => Scope::Whole { capability: String::from_str_in(capability, alloc), device: None },
            // The rest fill themselves (`Kind::fills_itself`, and `In`), or were refused as the
            // crate compiled (a whole `Io`, by `Setup::checked`).
            _ => continue,
        };
        let name = GrantName::new(param.name, alloc).map_err(|_| SetupError::GrantName { param: param.name })?;
        spec.put_grant(Granted { name, scope, provider: None, layers: None });
    }
    Ok(Spec::new(spec, alloc))
}

/// A fixture's file: its name in the simulated tree and its bytes.
pub(super) struct Laid {
    name: FileName,
    bytes: &'static [u8],
}

/// Every fixture's file of `setup`, named beneath its folder, once per test.
pub(super) fn laid(setup: &Setup, alloc: LaunchAlloc) -> Result<Vec<Laid, LaunchAlloc>, SetupError> {
    let mut laid = Vec::new_in(alloc);
    for dir in setup.dirs() {
        for file in dir.files {
            let mut full = String::new_in(alloc);
            full.str(dir.grant).str("/").str(file.path);
            let name = FileName::new(full.as_str()).ok_or(SetupError::FixturePath { grant: dir.grant, path: file.path })?;
            laid.push(Laid { name, bytes: file.bytes });
        }
    }
    Ok(laid)
}

/// The simulated world of `seed` for a test set up by `setup`: its environment and command
/// line, each fixture's file laid durably (as by a person before the program starts: the
/// seed's faults are armed after), the standard input given and ended.
pub(super) fn boot(seed: u64, setup: &Setup, laid: &[Laid]) -> Io {
    let mut world = SimWorld::new(seed, Limits::USUAL);
    // alloc: once per seed, SimWorld's own environment and command line (its fields' types).
    world.env = setup.env.unwrap_or(&[]).iter().map(|var| (var.name.into(), var.value.into())).collect();
    world.args = setup.args.unwrap_or(&[]).iter().map(|&arg| arg.into()).collect();
    let io = Io::simulated(world);
    let _: Option<()> = io.with_sim(|sim: &mut Sim| {
        for file in laid {
            sim.change(&file.name, file.bytes);
        }
        sim.stdin(setup.input.unwrap_or(&[]));
        sim.stdin_ends();
    });
    io
}

/// How one start of the test ended, from what its body gave or the unwind it ended by.
fn ended<R: Outcome>(io: &Io, ran: Result<Result<R, Refused>, alloc::boxed::Box<dyn core::any::Any + Send>>, alloc: LaunchAlloc) -> Ended {
    match ran {
        Ok(Ok(outcome)) => match outcome.gave_up() {
            None => Ended::Well,
            Some(error) => Ended::GaveUp(error),
        },
        Ok(Err(refused)) => {
            let mut text = String::new_in(alloc);
            refusal(&refused, &mut text);
            // alloc: a failure's message, in the form `Ended` holds.
            let text: alloc::string::String = text.as_str().into();
            // A grant's making that failed in the world (a folder's open, an injected fault)
            // is the program's start failing, as a process's start fails on a machine: given
            // up, so restarted under faults and a failure once they are healed. Any other
            // problem is the attribute and the signature disagreeing: a failure at once.
            match refused.problems.iter().all(|problem| matches!(problem, Problem::Param { want: Want::Failed(_), .. })) {
                true => Ended::GaveUp(text),
                false => Ended::Panicked(text),
            }
        }
        Err(payload) => match payload.downcast::<Exit>() {
            Ok(_) if io.with_sim(|sim| sim.is_off()) == Some(true) => Ended::PowerLost,
            Ok(exit) => match exit.status {
                0 => Ended::Well,
                // alloc: an error's message.
                status => Ended::GaveUp(alloc::format!("the process exited with status {status}")),
            },
            Err(payload) => Ended::Panicked(said(&*payload)),
        },
    }
}

/// A refusal of the test's parameters, in a test's words: no `--grant` line, since the
/// attribute is what grants.
fn refusal(refused: &Refused, out: &mut String<LaunchAlloc>) {
    out.str("bstd::test: the test's parameters could not be made:");
    for problem in refused.problems.iter() {
        match problem {
            Problem::Param { name, kind, want } => {
                out.str("\n- ").str(name).str(" (").str(kind.name()).str("): ");
                match want {
                    Want::NoGrant => out.str("not set up by the attribute"),
                    Want::WrongKind { given } => out.str("set up as a `").str(given).str("`"),
                    Want::Widens => out.str("set up read only, and it writes"),
                    Want::NotOffered(missing) => out.str("the test's world offers no `").str(missing.capability).str("`"),
                    Want::NotBuilt(what) => out.str("its `").str(what).str("` is not built yet"),
                    Want::Failed(said) => out.str(said.as_str()),
                    Want::NoneOf(_) => out.str("none of its alternatives is set up by the attribute"),
                };
            }
            Problem::NoParam { name, given } => {
                out.str("\n- ").str(name.as_str()).str(" (").str(given).str("): set up, and the test has no such parameter");
            }
        }
    }
}

/// One start of a test in `io`: a root context of [`BUDGET`], the launcher's [`Given`] over
/// `io` and `spec`, and `body` (which fills the parameters and calls the test) under
/// `catch_unwind`; how it ended ([`ended`]: a power cut's unwind, an exit, a panic, a refusal).
/// The one start every world of a test goes through, the drawn half's too: a test's entry, so
/// the heap a `heap: Heap` parameter is handed is the process's, from bstd's entry (D120).
pub(crate) fn start<B, R>(io: &Io, spec: &Spec, manifest: &'static Manifest, alloc: LaunchAlloc, body: &B) -> Ended
where
    B: Fn(&mut Given<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let cx = match io.cx(BUDGET, TASK_FRAME) {
        Ok(cx) => cx,
        // alloc: a failure's message.
        Err(stopped) => return Ended::Panicked(alloc::format!("bstd::test: a start's root context: {stopped}")),
    };
    let ran = {
        let mut given = Given::new(&cx, io, None, spec, Started::WithSpec, manifest, crate::entry::process_heap());
        catch_unwind(AssertUnwindSafe(|| body(&mut given)))
    };
    drop(cx);
    ended(io, ran, alloc)
}

/// A test's world for one seed: the simulated world, the test's spec and its body.
struct TestWorld<'p, B> {
    io: Io,
    spec: &'p Spec,
    manifest: &'static Manifest,
    body: &'p B,
    alloc: LaunchAlloc,
}

impl<B, R> World for TestWorld<'_, B>
where
    B: Fn(&mut Given<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    fn run(&mut self) -> Ended {
        start(&self.io, self.spec, self.manifest, self.alloc, self.body)
    }

    fn crash(&mut self) {
        self.io.crash();
    }

    fn with_sim<T>(&mut self, f: impl FnOnce(&mut Sim) -> T) -> T {
        match self.io.with_sim(f) {
            Some(it) => it,
            None => unreachable!("a bstd::test's world is Io::simulated's: simulated"),
        }
    }
}

/// What the person running the test gave, the ONE narrowing outside the test's code:
/// `BLAZE_SIM_SEED` and `BLAZE_SIM_SEEDS`, read from the test entry's own environment (the
/// process's start block, as `main`'s launcher reads it), never from `std::env`.
fn given_by_the_person() -> Result<SeedsGiven, alloc::string::String> {
    let block = crate::rt::process_start()?;
    let env = crate::env::Env::new(crate::env::Vars::process(block), crate::Plant::None);
    SeedsGiven::parse(var(&env, seeds::SEED_VAR)?, var(&env, seeds::SEEDS_VAR)?)
}

/// `BLAZE_SIM_TAPE` of the test entry's environment, as `given_by_the_person` reads the seed:
/// one case's tape as blaze-sim's text, for the drawn half.
#[cfg(any(test, feature = "sim-draws"))]
fn tape_by_the_person() -> Result<Option<alloc::string::String>, alloc::string::String> {
    let block = crate::rt::process_start()?;
    let env = crate::env::Env::new(crate::env::Vars::process(block), crate::Plant::None);
    // alloc: once per test, the tape's text out of the environment it is read from.
    Ok(var(&env, blaze_sim::search::TAPE)?.map(alloc::string::String::from))
}

/// A test with drawn parameters (D109): the person's seed, count and tape, the crate's kept
/// tapes when the attribute says `kept`, then simtest/drawn.rs; a failure panics with its block.
#[cfg(any(test, feature = "sim-draws"))]
fn drawn_entry<B, R>(test: &Test, alloc: LaunchAlloc, body: &B)
where
    B: Fn(&mut Given<'_>, &super::param::Draws<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let (world, tape) = match given_by_the_person().and_then(|world| tape_by_the_person().map(|tape| (world, tape))) {
        Ok(given) => given,
        Err(why) => panic!("bstd::test: {why}"),
    };
    let kept = test.setup.kept.map(|(text, krate)| blaze_sim::kept::File::new(text, krate));
    let given = super::drawn::DrawnGiven { world, tape: tape.as_deref() };
    if let Err(report) = super::drawn::run(test, given, kept, alloc, body) {
        panic!("{}", report.as_str());
    }
}

/// The variable `name` of the test entry's environment.
fn var<'e>(env: &'e crate::env::Env, name: &'static str) -> Result<Option<&'e str>, alloc::string::String> {
    // alloc: an error's message.
    let var = crate::env::VarName::new(name).map_err(|_| alloc::format!("{name} is no variable's name"))?;
    // alloc: an error's message.
    env.get(&var).map_err(|_| alloc::format!("{name} could not be read"))
}

/// What `#[bstd::test]` expands to when its parameters are grants: `body` (the launcher's,
/// [`launch_body!`](crate::launch_body)) run in each seed's world, or once (the module's doc);
/// a failure panics with [`report`]'s block, which the test harness prints. It is the test's
/// entry, so it takes the process's heap from bstd's entry (`entry::process_heap`, D120): the
/// runner's allocations are in it, and a `heap: Heap` parameter is handed it ([`start`]); the
/// attribute's expansion names no heap. It takes no `Os`: a simulated test touches no real OS,
/// and a `real` one is rooted in `bstd::test`'s own native world, so the attribute's expansion
/// names no root of real-OS authority (tests/reach.rs keeps that to the driver's backends).
pub fn granted<B, R>(test: Test, body: B)
where
    B: Fn(&mut Given<'_>, &super::param::Draws<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let alloc: LaunchAlloc = crate::entry::process_heap().shared();
    // A test with drawn parameters: the drawn half's runner (simtest/drawn.rs). Without
    // `sim-draws` no type is drawn (no blanket impl), so `drawn` is 0.
    #[cfg(any(test, feature = "sim-draws"))]
    if test.planned.drawn > 0 {
        return drawn_entry(&test, alloc, &body);
    }
    let body = |given: &mut Given<'_>| body(given, &super::param::Draws::NONE);
    if test.setup.real {
        return real(&test, &body, alloc);
    }
    // The person's seed and count narrow only a test that has seeds: one run once has none.
    let given = match test.planned.once {
        true => SeedsGiven::NOTHING,
        false => match given_by_the_person() {
            Ok(given) => given,
            Err(why) => panic!("bstd::test: {why}"),
        },
    };
    let broken = match simulated(&test, given, alloc, &body) {
        Ok(_) => return,
        Err(broken) => broken,
    };
    let mut text = String::new_in(alloc);
    match &broken.failed {
        Failed::Seed { run, at, of, replay, .. } => {
            report(&test, &listed(test.module, test.name), run, (*at, *of), replay, broken.last.as_ref(), &mut text);
        }
        other => {
            text.str("bstd::test: ");
            blaze_text::PutDisplay::put_display(&mut text, other);
        }
    }
    panic!("{}", text.as_str());
}

/// A test that did not pass in the simulation: how, and the last world it ran in (the
/// failing seed's second run), whose `out` and `err` the report shows.
pub struct Broken {
    pub failed: Failed,
    pub last: Option<Io>,
}

/// `test`'s `body` in the simulation, as a value: under its seeds (or the seed or count
/// `given` in their place), or once when no fault can reach it. What [`granted`] runs, and
/// what a test of the runner itself calls.
pub fn simulated<B, R>(test: &Test, given: SeedsGiven, alloc: LaunchAlloc, body: &B) -> Result<seeds::Passed, Broken>
where
    B: Fn(&mut Given<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let set_up = spec_of(test, alloc).and_then(|spec| laid(test.setup, alloc).map(|laid| (spec, laid)));
    let (spec, laid) = match set_up {
        Ok(set_up) => set_up,
        Err(why) => {
            let mut text = String::new_in(alloc);
            text.str("`").str(test.name).str("` could not be set up: ");
            why.line(&mut text);
            // alloc: a failure's message, in the form `Failed` holds.
            return Err(Broken { failed: Failed::Given(text.as_str().into()), last: None });
        }
    };
    let listed = listed(test.module, test.name);
    let name = Name { package: test.package, test: &listed };
    let mut last: Option<Io> = None;
    let mut world = |seed: u64| {
        let io = boot(seed, test.setup, &laid);
        // clone: the world's handle, kept for what `out` and `err` wrote if this seed fails.
        last = Some(io.clone());
        TestWorld { io, spec: &spec, manifest: test.manifest, body, alloc }
    };
    let ran = match test.planned.once {
        // No fault can reach it: one run, in a world with no fault, which every seed's is.
        true => {
            let run = seeds::one(0, &test.planned.seeds, &mut world);
            match run.verdict {
                Ok(()) => Ok(seeds::Passed { seeds: 1, starts: u64::from(run.starts), counts: run.counts }),
                Err(_) => Err(Failed::Seed {
                    package: test.package.into(),
                    // alloc: once per failure, its name and its run.
                    test: listed.clone(),
                    at: 1,
                    of: 1,
                    run: alloc::boxed::Box::new(run),
                    replay: Replay::Same,
                }),
            }
        }
        false => seeds::run(test.planned.seeds, given, name, &mut world),
    };
    ran.map_err(|failed| Broken { failed, last })
}

/// One seed of `test`'s `body`: its verdict, its counts and its trace, under the test's plan
/// (a test of the runner: one seed twice is one trace).
pub fn one_seed<B, R>(test: &Test, seed: u64, alloc: LaunchAlloc, body: &B) -> Result<Run, SetupError>
where
    B: Fn(&mut Given<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let spec = spec_of(test, alloc)?;
    let laid = laid(test.setup, alloc)?;
    let mut world = |seed: u64| TestWorld { io: boot(seed, test.setup, &laid), spec: &spec, manifest: test.manifest, body, alloc };
    Ok(seeds::one(seed, &test.planned.seeds, &mut world))
}

/// The test on the machine: its grants beneath a directory made for it, run once.
#[cfg(any(test, feature = "testing"))]
fn real<B, R>(test: &Test, body: &B, alloc: LaunchAlloc)
where
    B: Fn(&mut Given<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    fn could_not(test: &Test, said: &dyn core::fmt::Display, alloc: LaunchAlloc) -> ! {
        let mut text = String::new_in(alloc);
        text.str("bstd::test: `").str(test.name).str("` (real) could not be set up: ");
        blaze_text::PutDisplay::put_display(&mut text, said);
        panic!("{}", text.as_str());
    }
    let set_up = spec_of(test, alloc).and_then(|spec| laid(test.setup, alloc).map(|laid| (spec, laid)));
    let (spec, laid) = match set_up {
        Ok(set_up) => set_up,
        Err(why) => {
            let mut text = String::new_in(alloc);
            why.line(&mut text);
            could_not(test, &text.as_str(), alloc)
        }
    };
    let spec = &spec;
    // Ambient until batch P5-temp: TempDir reads TMPDIR itself; then the entry's Env hands it.
    let dir = match blaze_core::temp::TempDir::new("bstd-test-") {
        Ok(dir) => dir,
        Err(why) => could_not(test, &why, alloc),
    };
    let io = match crate::rt::testing_native::native(dir.path()) {
        Ok(io) => io,
        Err(why) => could_not(test, &why, alloc),
    };
    let cx = match io.cx(BUDGET, TASK_FRAME) {
        Ok(cx) => cx,
        Err(stopped) => could_not(test, &stopped, alloc),
    };
    let fs = match io.need_fs() {
        Ok(fs) => fs,
        Err(why) => could_not(test, &why, alloc),
    };
    for file in laid.iter() {
        let lay = crate::path::Path::new(file.name.as_str())
            .map_err(|why| alloc::format!("{why}"))
            .and_then(|path| fs.root().publish_synced(&cx, &path, crate::fs::Content::bytes(file.bytes)).map_err(|why| alloc::format!("{why}")));
        if let Err(why) = lay {
            could_not(test, &why, alloc);
        }
    }
    // The fixture's context ends before the test's own start makes its root.
    drop(cx);
    let why = match start(&io, spec, test.manifest, alloc, body) {
        Ended::Well => return,
        Ended::GaveUp(why) => why,
        Ended::Panicked(why) => why,
        Ended::PowerLost => unreachable!("a real test's world has no simulated power to cut"),
    };
    let mut text = String::new_in(alloc);
    text.str("`").str(&listed(test.module, test.name)).str("` failed, on the machine (`real`): NOT REPLAYABLE BY SEED\n");
    takes(test.manifest, &mut text);
    text.str("why: ").str(&why).str("\n");
    text.str("run again: blaze test -p ").str(test.package).str(" ").str(&listed(test.module, test.name)).str(" -- --exact");
    panic!("{}", text.as_str());
}

#[cfg(not(any(test, feature = "testing")))]
fn real<B, R>(test: &Test, _: &B, alloc: LaunchAlloc)
where
    B: Fn(&mut Given<'_>) -> Result<R, Refused>,
    R: Outcome,
{
    let mut text = String::new_in(alloc);
    text.str("bstd::test: `").str(test.name).str("`: ");
    SetupError::RealNeedsTesting.line(&mut text);
    panic!("{}", text.as_str());
}

/// `takes: data: dir (writes), clock: clock` (or `takes: cx alone`).
fn takes(manifest: &Manifest, out: &mut String<LaunchAlloc>) {
    out.str("takes: cx");
    if manifest.params.is_empty() {
        out.str(" alone");
    }
    for param in manifest.params {
        out.str(", ").str(param.name).str(": ").str(param.kind.name());
        if let Kind::Dir { write } = param.kind {
            out.str(if write { " (writes)" } else { " (reads)" });
        }
    }
    out.str("\n");
}

/// The fault kinds in `faults`, by name: `IoFailures, Delays` (or `none`).
fn fault_names(faults: u8, out: &mut String<LaunchAlloc>) {
    let names = [(Fault::IoFailures, "IoFailures"), (Fault::Delays, "Delays"), (Fault::Crashes, "Crashes")];
    let mut first = true;
    for (fault, name) in names {
        if faults & bit(fault) != 0 {
            if !first {
                out.str(", ");
            }
            out.str(name);
            first = false;
        }
    }
    if first {
        out.str("none: it is run once");
    }
}

/// The bytes a stream got, as text, or how many bytes that are not text.
fn stream(label: &str, bytes: &[u8], out: &mut String<LaunchAlloc>) {
    if bytes.is_empty() {
        return;
    }
    out.str(label).str(" (").int(bytes.len()).str(" bytes):\n");
    match core::str::from_utf8(bytes) {
        Ok(text) => {
            for line in text.lines() {
                out.str("  ").str(line).str("\n");
            }
        }
        Err(_) => {
            out.str("  (not UTF-8)\n");
        }
    }
}

/// The failure of a test under seeds, as one block (D109): its name; what it took; the seed,
/// its place among the seeds and the faults in play with the plan's draws; why it broke (a
/// panic's text holds the first failing check and its values); whether the seed replays;
/// what `out` and `err` wrote; the trace's last lines; ONE line that replays it.
///
/// Before D109 (`seeds::Failed`'s `Display`): the first line said seed, start and why; then
/// `plan: Plan { .. }` as Rust's debug form; the counts; the replay check; the trace's last 24
/// lines; the replay line. It said nothing of what the test took, which faults were in play by
/// name, or what the program printed.
pub fn report(
    test: &Test,
    listed: &str,
    run: &Run,
    (at, of): (u32, u32),
    replay: &Replay,
    world: Option<&Io>,
    out: &mut String<LaunchAlloc>,
) {
    report_drawn(test, listed, run, (at, of), replay, world, None, out);
}

/// [`report`] of a case with drawn parameters (D109): `drawn` adds each drawn parameter's
/// shrunk value and the tape, and the replay line gives the tape beside the seed.
#[expect(clippy::too_many_arguments, reason = "report's seven and the drawn block, one call site each")]
pub fn report_drawn(
    test: &Test,
    listed: &str,
    run: &Run,
    (at, of): (u32, u32),
    replay: &Replay,
    world: Option<&Io>,
    drawn: Option<&super::param::DrawnReport<'_>>,
    out: &mut String<LaunchAlloc>,
) {
    let plan = &run.plan;
    let (start, why) = match &run.verdict {
        Err(broke) => (broke.start, broke.why.as_str()),
        Ok(()) => (run.starts, "it passed"),
    };
    out.str("`").str(listed).str("` of ").str(test.package).str(" FAILED\n");
    takes(test.manifest, out);
    match test.planned.once {
        true => {
            out.str("run once: no fault can reach what it takes\n");
        }
        false => {
            out.str("seed ").int(plan.seed).str(" (").int(at).str(" of ").int(of).str("), start ").int(start).str(" of ").int(run.starts).str("\n");
            out.str("faults in play: ");
            fault_names(test.planned.faults, out);
            out.str(" (a failure 1 in ").int(plan.fail_one_in).str(", late by up to ").int(u64::try_from(plan.max_latency.as_micros()).unwrap_or(u64::MAX)).str(" us, a power cut 1 in ");
            out.int(plan.crash_one_in).str(", at most ").int(plan.cuts_most).str(" cuts; 0: none)\n");
        }
    }
    if let Some(drawn) = drawn {
        out.str(drawn.values);
        out.str("tape: ").str(drawn.tape).str("\n");
    }
    out.str("why: ").str(why).str("\n");
    out.str("did: ").int(run.counts.effects).str(" effects, ").int(run.counts.failures).str(" failed, ").int(run.counts.cuts).str(" power cuts\n");
    match replay {
        Replay::Same => {
            out.str("the seed replays: a second run gave the same trace, byte for byte\n");
        }
        Replay::Diverged { line, first, second } => {
            out.str("THE SEED DOES NOT REPLAY: the second run differs from trace line ").int(*line);
            out.str(" (the program or its world takes a choice from outside the seed)\n  first:  ").str(first).str("\n  second: ").str(second).str("\n");
        }
    }
    // What `Out` wrote is the simulated host's (`SimHost::out`, `err`: io.rs `write_out`), not
    // the simulation's own streams (`Sim::output`, which only a program on blaze-core's sim
    // host writes).
    if let Some(io) = world {
        let _: Option<()> = io.script(|host| {
            stream("out", &host.out, out);
            stream("err", &host.err, out);
        });
    }
    let lines = run.trace.lines().count();
    let tail = lines.min(24);
    out.str("the trace's last ").int(tail).str(" of ").int(lines).str(" lines:\n");
    for line in run.trace.lines().skip(lines - tail) {
        out.str("  ").str(line).str("\n");
    }
    out.str("replay: ");
    if !test.planned.once {
        out.str(seeds::SEED_VAR).str("=").int(plan.seed).str(" ");
    }
    if let Some(drawn) = drawn {
        // The tape's text holds spaces: one shell word in single quotes (it holds none).
        out.str("BLAZE_SIM_TAPE='").str(drawn.tape).str("' ");
    }
    out.str("blaze test -p ").str(test.package).str(" ").str(listed).str(" -- --exact");
}

/// Which parameters an attribute argument may name.
#[derive(Clone, Copy)]
enum KindClass {
    Dir,
    Net,
    Env,
    Args,
    In,
}

const fn in_class(kind: Kind, class: KindClass) -> bool {
    match class {
        KindClass::Dir => matches!(kind, Kind::Dir { .. }),
        KindClass::Net => matches!(kind, Kind::Net),
        KindClass::Env => matches!(kind, Kind::Env),
        KindClass::Args => matches!(kind, Kind::Args),
        // By its manifest name until P7's `Kind::In` is named here.
        KindClass::In => same(kind.name(), "in"),
    }
}

/// Whether `params` has one named `name` of `class`.
const fn has_kind(params: &[Param], name: &str, class: KindClass) -> bool {
    let mut i = 0;
    while i < params.len() {
        if same(params[i].name, name) && in_class(params[i].kind, class) {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether `params` has one of `class`.
const fn has_class(params: &[Param], class: KindClass) -> bool {
    let mut i = 0;
    while i < params.len() {
        if in_class(params[i].kind, class) {
            return true;
        }
        i += 1;
    }
    false
}

/// A path beneath a folder: not empty, not absolute, no empty, `.` or `..` part.
const fn relative(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes[0] == b'/' {
        return false;
    }
    let (mut start, mut at) = (0, 0);
    while at <= bytes.len() {
        if at == bytes.len() || bytes[at] == b'/' {
            let len = at - start;
            if len == 0 || (len == 1 && bytes[start] == b'.') || (len == 2 && bytes[start] == b'.' && bytes[start + 1] == b'.') {
                return false;
            }
            start = at + 1;
        }
        at += 1;
    }
    true
}

/// The attribute's arguments as a [`Setup`] (`#[bstd::test(..)]`, [`crate::test!`]): a
/// tt-muncher, one rule an argument, any order, comma separated. An argument it does not take
/// is a compile error listing those it takes.
#[doc(hidden)]
#[macro_export]
macro_rules! __test_setup {
    (@ $s:expr ;) => { $s };
    (@ $s:expr ; cases = $n:literal $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.cases($n) ; $($($rest)*)?)
    };
    (@ $s:expr ; kept $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.kept(
            ::core::include_str!(::core::concat!(::core::env!("CARGO_MANIFEST_DIR"), "/sim-kept.tapes")),
            ::core::env!("CARGO_PKG_NAME"),
        ) ; $($($rest)*)?)
    };
    (@ $s:expr ; real $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.real() ; $($($rest)*)?)
    };
    (@ $s:expr ; seed = $seed:literal $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.seed($seed) ; $($($rest)*)?)
    };
    (@ $s:expr ; without($($fault:ident),+ $(,)?) $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s $(.without($crate::test::Fault::$fault))+ ; $($($rest)*)?)
    };
    (@ $s:expr ; env($($var:ident = $value:literal),* $(,)?) $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.env(&[$($crate::test::Var::new(::core::stringify!($var), $value)),*]) ; $($($rest)*)?)
    };
    (@ $s:expr ; args($($arg:literal),* $(,)?) $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.args(&[$($arg),*]) ; $($($rest)*)?)
    };
    (@ $s:expr ; input = $bytes:expr $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.input($bytes) ; $($($rest)*)?)
    };
    (@ $s:expr ; $grant:ident = files($($path:literal => $content:expr),* $(,)?) $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.files(::core::stringify!($grant), &[$($crate::test::File::new($path, $content)),*]) ; $($($rest)*)?)
    };
    (@ $s:expr ; $grant:ident = connect($at:literal) $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.net(::core::stringify!($grant), $at, false) ; $($($rest)*)?)
    };
    (@ $s:expr ; $grant:ident = listen($at:literal) $(, $($rest:tt)*)?) => {
        $crate::__test_setup!(@ $s.net(::core::stringify!($grant), $at, true) ; $($($rest)*)?)
    };
    (@ $s:expr ; $($other:tt)*) => {
        ::core::compile_error!(::core::concat!(
            "bstd::test: an argument it does not take: `", ::core::stringify!($($other)*),
            "`; it takes `seed = N`, `without(IoFailures, Delays, Crashes)`, `real`, `cases = N`, `kept`, ",
            "`<dir> = files(\"path\" => b\"..\", ..)`, `<net> = connect(\"host:port\")` or `listen(..)`, ",
            "`env(NAME = \"value\", ..)`, `args(\"..\", ..)`, `input = b\"..\"`"
        ))
    };
}
