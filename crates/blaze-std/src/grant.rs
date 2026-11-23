//! What `main` takes, said in its signature (docs/v2/plan/design/run.md, "What a program is
//! given"): after `cx: &Cx`, each parameter is a grant, its TYPE the kind of capability and its
//! NAME the grant's name.
//!
//! ```ignore
//! #[bstd::main]
//! fn main(cx: &Cx, out: Out, data: Dir<ReadOnly>, api: Net) -> Result<(), Error> { .. }
//! ```
//!
//! - [`Grant`] is the trait every type that may stand there implements: its [`Kind`], whether
//!   it fills itself ([`Kind::fills_itself`]: the command line, standard output and error, the
//!   clock, memory, the program's own spec; nothing that reaches outside the process), and how
//!   it is made from the spec's grant of its name ([`Granting`]). A type that is not one is a
//!   compile error at that parameter's type, which names the type and lists the kinds.
//! - **Standard output and standard error are both [`Out`]; the parameter's name says which**:
//!   `out` is standard output, `err` standard error, and an `Out` of any other name is a
//!   compile error ([`Param::new`] is evaluated as the crate compiles). One type for one kind,
//!   and the name is what the spec and the command line say anyway.
//! - **A whole world cannot be asked for** (D104): [`Io`] is not a `Grant`. A `main` names each
//!   capability it uses, so its manifest says what it reaches and anything else can be withheld;
//!   `fn main(cx: &Cx, io: Io)` is a compile error at `Io`, whose note names what to write.
//!   `Io` stays bstd's own world handle (the launcher makes every grant from it) and a test's
//!   simulated world.
//! - [`Manifest`] is the program's list of parameters, each one's name and kind: a `static` in
//!   the binary, printed as JSON when file descriptor 3 asks (`bstd manifest`), so a tool learns
//!   what a built program takes without running its `main`.
//! - [`Given`] is the launcher's side: every parameter is made from the spec before `main`
//!   runs, ONCE: its check is its making, and `main` is handed what the check made
//!   ([`Checking`], [`Filled`]); every problem (no grant, a grant of the wrong kind, one that would widen, one for a
//!   parameter `main` lacks, a capability the host does not offer) is collected, and then all
//!   are printed at once, each with the line that fixes it, and the program exits
//!   [`Status::REFUSED`] without starting `main`. Nothing prompts.

use core::{alloc::Allocator, fmt};

use blaze_data::{Int, Map, Value};
use blaze_json::WriteOptions;
use blaze_own::{SharedAlloc, String, Vec};
use blaze_text::{Writes as _, text_in};

use crate::{
    cx::Cx,
    entry::{Outcome, Status},
    fs::{Dir, ReadOnly, ReadWrite},
    io::{Io, Missing},
    net::{Addr, HostPort},
    out::Out,
    path::Path,
    spec::{Granted, Layers, Provider, RunSpec, Scope},
};

mod compose;
mod defaults;
mod made;
mod one_device;

pub use compose::{
    Alternatives, Chosen2, Chosen3, Chosen4, Connect, Form, Listen, Many, Maybe, Narrow, OneOf, START_FOLDER, is_grant_name, member_of,
};
pub use defaults::{OrdinaryError, Shape, Spelling, ordinary_grants};
pub use made::{Checking, Filled};
pub use one_device::{BY_MACHINE, ONE_DEVICE, is_whole_capability, narrows_to_device};

/// The alternative a `--grant` value names, for spec.rs `grant_arg` (compose.rs `alternative_of`).
pub fn compose_alternative<'v>(kinds: &[Kind], value: &'v str) -> Option<(Kind, &'v str)> {
    compose::alternative_of(kinds, value)
}

/// The scope a `--grant` value gives a kind, for spec.rs `grant_arg` (compose.rs `scope_of`).
pub fn compose_scope<A: Allocator + Clone>(
    kind: Kind,
    narrow: Narrow,
    value: &str,
    alloc: A,
) -> Result<Scope<A>, crate::spec::GrantArgError> {
    compose::scope_of(kind, narrow, value, alloc)
}

/// The allocator a launcher's spec and problems are in: the heap's handle, which the entry
/// (`rt::launch`, or a test through [`crate::entry::launch_in`]) hands down (D102).
pub type LaunchAlloc = SharedAlloc<'static>;

/// What a capability parameter of `main` is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The command line ([`crate::args::Args`]). Fills itself.
    Args,
    /// Standard output (named `out`) or standard error (named `err`). Fills itself.
    Out,
    /// Standard input ([`crate::input::In`]), named `input` (`in` is Rust's keyword). Fills
    /// itself: the process's own stream reaches nothing outside it.
    In,
    /// The clock ([`crate::clock::Clock`]). Fills itself.
    Clock,
    /// Named arenas ([`crate::mem::Mem`]). Fills itself.
    Mem,
    /// The program's own run spec ([`Spec`]). Fills itself.
    Spec,
    /// The process's heap ([`crate::mem::Heap`]), handed down by the entry (D120). Fills
    /// itself: it reaches nothing outside the process.
    Heap,
    /// A folder ([`Dir`]): read only, or read and write.
    Dir { write: bool },
    /// One network endpoint ([`crate::net::Net`]), connected to or listened on.
    Net,
    /// Named environment variables ([`crate::env::Env`]).
    Env,
    /// One root capability whole, by its name in [`crate::io::Offers::NAMES`].
    Whole(&'static str),
}

impl Kind {
    /// Whether a parameter of this kind is filled with no grant: it reaches nothing outside
    /// the process.
    pub const fn fills_itself(self) -> bool {
        matches!(self, Kind::Args | Kind::Out | Kind::In | Kind::Clock | Kind::Mem | Kind::Spec | Kind::Heap)
    }

    /// The kinds that fill themselves ([`Kind::fills_itself`]).
    pub const FILLS_ITSELF: [Kind; 7] = [Kind::Args, Kind::Out, Kind::In, Kind::Clock, Kind::Mem, Kind::Spec, Kind::Heap];

    /// Whether the kind named `name` in the manifest's JSON (and on the hosted wire) fills
    /// itself: what a host answers with no grant (crate::hosting's `grant`).
    pub fn fills_itself_named(name: &[u8]) -> bool {
        Kind::FILLS_ITSELF.iter().any(|kind| kind.name().as_bytes() == name)
    }

    /// Its name in the manifest's JSON.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Args => "args",
            Kind::Out => "out",
            Kind::In => "in",
            Kind::Clock => "clock",
            Kind::Mem => "mem",
            Kind::Spec => "spec",
            Kind::Heap => "heap",
            Kind::Dir { .. } => "dir",
            Kind::Net => "net",
            Kind::Env => "env",
            Kind::Whole(_) => "whole",
        }
    }

    /// What follows `--grant <name>=` for it: what the fix line shows.
    pub const fn scope_form(self) -> &'static str {
        match self {
            Kind::Dir { .. } => "<folder>",
            Kind::Net => "<host>:<port>",
            Kind::Env => "<NAME>,<NAME>",
            Kind::Whole(_) => "whole",
            Kind::Args | Kind::Out | Kind::In | Kind::Clock | Kind::Mem | Kind::Spec | Kind::Heap => "",
        }
    }

    /// The type a person wrote for it, for messages.
    const fn ty(self) -> &'static str {
        match self {
            Kind::Args => "Args",
            Kind::Out => "Out",
            Kind::In => "In",
            Kind::Clock => "Clock",
            Kind::Mem => "Mem",
            Kind::Spec => "Spec",
            Kind::Heap => "Heap",
            Kind::Dir { write: false } => "Dir<ReadOnly>",
            Kind::Dir { write: true } => "Dir<ReadWrite>",
            Kind::Net => "Net",
            Kind::Env => "Env",
            Kind::Whole(name) => name,
        }
    }
}

/// One parameter of `main`: its name, its kind, and how it takes its grants (compose.rs: one,
/// optional, one of, many; a network direction). [`Param::of`] reads all from the type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Param {
    pub name: &'static str,
    pub kind: Kind,
    pub form: Form,
    pub narrow: Narrow,
}

/// Byte equality, in a const fn.
pub(crate) const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut at = 0;
    while at < a.len() {
        if a[at] != b[at] {
            return false;
        }
        at += 1;
    }
    true
}

impl Param {
    /// The parameter `name` of `kind`, checked as the crate compiles: an [`Out`] is named
    /// `out` or `err`, and every name is a grant's name ([`crate::spec::is_name`]).
    pub const fn new(name: &'static str, kind: Kind) -> Param {
        if matches!(kind, Kind::Out) && !same(name, "out") && !same(name, "err") {
            panic!("an `Out` parameter of `main` is named `out` (standard output) or `err` (standard error)");
        }
        if matches!(kind, Kind::In) && !same(name, "input") {
            panic!("an `In` parameter of `main` (standard input) is named `input`");
        }
        if !crate::spec::is_name(name) {
            panic!("a parameter of `main` is named by ASCII letters, digits and `_`, at most 64 bytes");
        }
        Param { name, kind, form: Form::One, narrow: Narrow::None }
    }
}

/// A program's manifest: its parameters after `cx`, in order. `#[bstd::main]` makes it a
/// `static`, so it is data in the binary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Manifest {
    pub params: &'static [Param],
    /// The attribute's defaults of an ordinary start (defaults.rs), checked by
    /// [`Manifest::with_defaults`]; none unless the attribute spells some.
    pub defaults: &'static [Spelling],
}

impl Manifest {
    pub const fn new(params: &'static [Param]) -> Manifest {
        Manifest { params, defaults: &[] }
    }

    /// The parameter named `name`.
    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|param| param.name == name)
    }

    /// The manifest as compact JSON, keys sorted:
    /// `{"params":[{"fills_itself":false,"kind":"dir","name":"data","write":false}],"version":1}`.
    pub fn write<A: Allocator + Clone>(&self, a: A) -> String<A> {
        let mut params = Vec::with_capacity_in(self.params.len(), a.clone());
        for param in self.params {
            let mut map = Map::new_in(a.clone());
            let key = |k: &str| String::from_str_in(k, a.clone());
            map.insert(key("name"), Value::Text(key(param.name)));
            map.insert(key("kind"), Value::Text(key(param.kind.name())));
            map.insert(key("fills_itself"), Value::Bool(param.kind.fills_itself()));
            match param.kind {
                Kind::Dir { write } => {
                    map.insert(key("write"), Value::Bool(write));
                }
                Kind::Whole(capability) => {
                    map.insert(key("capability"), Value::Text(key(capability)));
                }
                Kind::Args | Kind::Out | Kind::In | Kind::Clock | Kind::Mem | Kind::Spec | Kind::Heap | Kind::Net | Kind::Env => {}
            }
            compose::write_shape(param, &mut map, a.clone());
            params.push(Value::Map(map));
        }
        let mut root = Map::new_in(a.clone());
        root.insert(String::from_str_in("version", a.clone()), Value::Int(Int::from_u64(crate::spec::VERSION)));
        root.insert(String::from_str_in("params", a.clone()), Value::List(params));
        match blaze_json::to_string_in(&Value::Map(root), WriteOptions::COMPACT, a) {
            Ok(text) => text,
            Err(_) => unreachable!("a manifest's value is text, booleans, integers, lists and maps"),
        }
    }
}

/// How the program was started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Started {
    /// With no run spec on file descriptor 3: an ordinary start, from a shell. Only what
    /// fills itself, and [`Io`], is filled.
    Ordinary,
    /// With a run spec: every grant is the spec's.
    WithSpec,
}

/// The program's own run spec, as a parameter: `spec: Spec`. Fills itself.
#[derive(Clone)]
pub struct Spec(blaze_own::Arc<RunSpec<LaunchAlloc>, LaunchAlloc>);

impl Spec {
    pub fn new(spec: RunSpec<LaunchAlloc>, alloc: LaunchAlloc) -> Spec {
        Spec(blaze_own::Arc::new_in(spec, alloc))
    }

    pub fn get(&self) -> &RunSpec<LaunchAlloc> {
        &self.0
    }

    /// The allocator the spec is in: the launcher's, which its entry handed down (D102).
    pub fn alloc(&self) -> LaunchAlloc {
        *blaze_own::Arc::allocator(&self.0)
    }
}

impl fmt::Debug for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.get(), f)
    }
}

/// Why one parameter could not be made.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Want {
    /// No grant of its name.
    NoGrant,
    /// A grant of its name, of another kind (its scope's name).
    WrongKind { given: &'static str },
    /// A read-only folder granted to a read-write parameter: it would widen.
    Widens,
    /// The host does not offer the capability.
    NotOffered(Missing),
    /// A grant whose provider or layer is not built yet (its name).
    NotBuilt(&'static str),
    /// The grant was there and right, and making the capability failed: what was said.
    Failed(String<LaunchAlloc>),
    /// A `OneOf` none of whose alternatives (these kinds, in written order) is granted.
    NoneOf(&'static [Kind]),
}

/// One problem the launcher found: the parameter (or a grant's name), its kind, and what was
/// wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Problem {
    Param { name: &'static str, kind: Kind, want: Want },
    /// A grant for a parameter `main` does not have.
    NoParam { name: String<LaunchAlloc>, given: &'static str },
}

impl Problem {
    /// The problem as one line, with the line that fixes it.
    fn line(&self, out: &mut String<LaunchAlloc>) {
        match self {
            Problem::Param { name, kind, want } => {
                out.str("- ").str(name).str(" (").str(kind.ty()).str("): ");
                let fix = |out: &mut String<LaunchAlloc>| {
                    out.str("add `--grant ").str(name).str("=").str(kind.scope_form()).str("`");
                };
                match want {
                    Want::NoGrant => {
                        out.str("no grant; ");
                        fix(out);
                    }
                    Want::WrongKind { given } => {
                        out.str("granted a `").str(given).str("`, and it takes a `").str(kind.name()).str("`; ");
                        fix(out);
                    }
                    Want::Widens => {
                        out.str("granted read only, and it writes; ");
                        fix(out);
                        out.str(", which grants what it asks");
                    }
                    Want::NotOffered(missing) => {
                        out.str("this host offers no `").str(missing.capability).str("`: run it where one is offered");
                    }
                    Want::NotBuilt(what) => {
                        out.str("its `").str(what).str("` is not built yet: grant it with the real provider and no layers");
                    }
                    Want::Failed(said) => {
                        out.str(said.as_str());
                    }
                    Want::NoneOf(kinds) => compose::none_of_fix(name, kinds, out),
                }
            }
            Problem::NoParam { name, given } => {
                out.str("- ").str(name.as_str()).str(" (").str(given).str("): granted, and `main` has no parameter `");
                out.str(name.as_str()).str("`; remove its `--grant ").str(name.as_str()).str("=..`");
            }
        }
    }
}

/// What the launcher said instead of starting `main`: every problem, each with its fix.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refused {
    pub problems: Vec<Problem, LaunchAlloc>,
    pub started: Started,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The launcher's allocator, which the problems are in (D102: never one from nowhere).
        let alloc = *self.problems.allocator();
        let text = text_in(alloc, |out| {
            out.str("bstd: `main` was not started: ").int(self.problems.len());
            match self.problems.len() {
                1 => out.str(" grant is missing or wrong"),
                _ => out.str(" grants are missing or wrong"),
            };
            if self.started == Started::Ordinary {
                out.str(" (started with no run spec on file descriptor 3: start it with `blaze run <package> --grant ..`)");
            }
            out
        });
        f.write_str(text.as_str())?;
        for problem in self.problems.iter() {
            let mut line = String::new_in(alloc);
            problem.line(&mut line);
            f.write_str("\n")?;
            f.write_str(line.as_str())?;
        }
        Ok(())
    }
}

/// What a `main` run by the launcher ends as: what it returned, or the launcher's refusal.
pub enum Launched<R> {
    Ran(R),
    Refused(Refused),
}

/// A refusal is [`Status::REFUSED`], its words the refusal's.
impl<R: Outcome> Outcome for Launched<R> {
    fn status<W: fmt::Write + ?Sized>(self, said: &mut W) -> Status {
        match self {
            Launched::Ran(ran) => ran.status(said),
            Launched::Refused(refused) => {
                let _ = write!(said, "{refused}");
                Status::REFUSED
            }
        }
    }
}

/// What one parameter is made from: the context `main` runs in, the parameter's name, and the
/// launcher's worlds and spec.
pub struct Granting<'g> {
    pub cx: &'g Cx,
    pub name: &'static str,
    /// The grant's own name when it is not the parameter's: a [`Many`]'s member (`inputs.a`).
    member: Option<&'g str>,
    given: &'g Given<'g>,
}

impl<'g> Granting<'g> {
    /// The launcher's allocator, the spec's (D102: handed down from the entry).
    pub fn alloc(&self) -> LaunchAlloc {
        self.given.spec.alloc()
    }

    /// The spec's grant of this parameter's name (of the member's, for a [`Many`]'s member).
    pub fn granted(&self) -> Option<&'g Granted<LaunchAlloc>> {
        self.given.spec.get().grant(self.member.unwrap_or(self.name))
    }

    /// How the program was started.
    pub fn started(&self) -> Started {
        self.given.started
    }

    /// The program's own spec.
    pub fn spec(&self) -> &'g Spec {
        self.given.spec
    }

    /// The process's heap, as the entry handed it to the launcher (D120).
    pub fn heap(&self) -> crate::mem::Heap {
        self.given.heap
    }

    /// The process's world: where its command line, its output and its error are.
    pub fn process(&self) -> &'g Io {
        self.given.real
    }

    /// The world of the run-wide provider: what a parameter that fills itself and reaches
    /// effects (the clock, memory) is made from.
    pub fn run_world(&self) -> &'g Io {
        match self.given.spec.get().provider {
            Provider::Fake => self.given.fake.unwrap_or(self.given.real),
            Provider::Real | Provider::Replay => self.given.real,
        }
    }

    /// The scope of this parameter's grant if it is of `kind` (named as its JSON key), and
    /// the world of its provider; else what is wrong.
    pub fn scope(&self, kind: &'static str) -> Result<(&'g Scope<LaunchAlloc>, &'g Io), Want> {
        let granted = self.granted().ok_or(Want::NoGrant)?;
        if granted.scope.kind_name() != kind {
            return Err(Want::WrongKind { given: granted.scope.kind_name() });
        }
        let spec = self.given.spec.get();
        if granted.layers.unwrap_or(spec.layers) != Layers::NONE {
            return Err(Want::NotBuilt("layer"));
        }
        let io = match granted.provider.unwrap_or(spec.provider) {
            Provider::Real => self.given.real,
            Provider::Fake => self.given.fake.ok_or(Want::NotBuilt("fake provider"))?,
            Provider::Replay => return Err(Want::NotBuilt("replay provider")),
        };
        Ok((&granted.scope, io))
    }
}

/// A type `main` may take after `cx` (the module's doc). `Io` is not one (D104): the note says
/// what to write instead, at the parameter.
#[diagnostic::on_unimplemented(
    message = "`main` cannot take a `{Self}`",
    label = "not a capability `main` may take",
    note = "a `main` names each capability it uses; a whole world (`Io`) cannot be asked for. Write \
            `out: Out` (standard output), `err: Out` (standard error), `args: Args`, `clock: Clock`, \
            `mem: Mem`, `heap: Heap` (the process's heap), `spec: Spec` or `input: In` (standard input), which fill themselves; or, granted by name, `data: Dir<ReadOnly>` \
            or `Dir<ReadWrite>` (a folder), `api: Net` (one endpoint), `env: Env` (named variables), \
            or a whole capability (`Fs`, `Rng`, `Host`, `Http`, ...) (bstd::grant::Grant)"
)]
pub trait Grant: Sized {
    const KIND: Kind;

    /// How it takes its grants (compose.rs): one, needed, unless it is a `Maybe`, `OneOf` or
    /// `Many`, whose form is checked here as the crate compiles.
    const FORM: Form = Form::One;

    /// The network direction its type says ([`Connect`], [`Listen`]).
    const NARROW: Narrow = Narrow::None;

    /// The capability, made from what the launcher holds.
    fn grant(from: &Granting<'_>) -> Result<Self, Want>;

    /// The capability `name`, made from a hosted world once its host granted it
    /// ([`crate::guest::grant`]); `None` for a kind the hosted wire does not carry yet.
    fn hosted(io: &Io, name: &str) -> Option<Self> {
        let _ = (io, name);
        None
    }
}

impl Grant for Out {
    const KIND: Kind = Kind::Out;

    fn grant(from: &Granting<'_>) -> Result<Out, Want> {
        match from.name {
            "err" => from.process().need_err().map_err(Want::NotOffered),
            _ => from.process().need_out().map_err(Want::NotOffered),
        }
    }

    fn hosted(io: &Io, name: &str) -> Option<Out> {
        match name {
            "err" => io.err(),
            _ => io.out(),
        }
    }
}

impl Grant for crate::args::Args {
    const KIND: Kind = Kind::Args;

    fn grant(from: &Granting<'_>) -> Result<crate::args::Args, Want> {
        from.process().need_args().map_err(Want::NotOffered)
    }

    fn hosted(io: &Io, _: &str) -> Option<crate::args::Args> {
        io.args()
    }
}

impl Grant for crate::clock::Clock {
    const KIND: Kind = Kind::Clock;

    fn grant(from: &Granting<'_>) -> Result<crate::clock::Clock, Want> {
        from.run_world().need_clock().map_err(Want::NotOffered)
    }

    fn hosted(io: &Io, _: &str) -> Option<crate::clock::Clock> {
        io.clock()
    }
}

impl Grant for crate::mem::Mem {
    const KIND: Kind = Kind::Mem;

    fn grant(from: &Granting<'_>) -> Result<crate::mem::Mem, Want> {
        from.run_world().need_mem().map_err(Want::NotOffered)
    }
}

impl Grant for Spec {
    const KIND: Kind = Kind::Spec;

    fn grant(from: &Granting<'_>) -> Result<Spec, Want> {
        // clone: the spec's handle, a refcount.
        Ok(from.spec().clone())
    }
}

/// The process's heap: the one the entry took and handed the launcher (D120), never a heap
/// reached for here. A `main` that does not name it is handed none.
impl Grant for crate::mem::Heap {
    const KIND: Kind = Kind::Heap;

    fn grant(from: &Granting<'_>) -> Result<crate::mem::Heap, Want> {
        Ok(from.heap())
    }

    /// The hosted run's heap, which its start (`guest::start`) was handed by bstd's entry.
    fn hosted(_io: &Io, _: &str) -> Option<crate::mem::Heap> {
        Some(crate::guest::heap())
    }
}

/// The folder of a `dir` grant, read and write, from its provider's world.
fn granted_dir(from: &Granting<'_>, wants_write: bool) -> Result<Dir<ReadWrite>, Want> {
    let (scope, io) = from.scope("dir")?;
    let Scope::Dir { path, write } = scope else { return Err(Want::WrongKind { given: scope.kind_name() }) };
    if wants_write && !*write {
        return Err(Want::Widens);
    }
    let fs = io.need_fs().map_err(Want::NotOffered)?;
    let failed = |why: &dyn fmt::Display| {
        let mut said = String::new_in(from.alloc());
        blaze_text::PutDisplay::put_display(&mut said, why);
        Want::Failed(said)
    };
    if path.as_str() == START_FOLDER {
        return Ok(fs.root());
    }
    let path = Path::new(path.as_str()).map_err(|why| failed(&why))?;
    fs.open_dir(from.cx, &path).map_err(|why| failed(&why))
}

impl Grant for Dir<ReadOnly> {
    const KIND: Kind = Kind::Dir { write: false };

    /// A folder granted read and write is handed on read only: narrowed, never widened.
    fn grant(from: &Granting<'_>) -> Result<Dir<ReadOnly>, Want> {
        granted_dir(from, false).map(|dir| dir.read_only())
    }
}

impl Grant for Dir<ReadWrite> {
    const KIND: Kind = Kind::Dir { write: true };

    fn grant(from: &Granting<'_>) -> Result<Dir<ReadWrite>, Want> {
        granted_dir(from, true)
    }
}

impl Grant for crate::net::Net {
    const KIND: Kind = Kind::Net;

    fn grant(from: &Granting<'_>) -> Result<crate::net::Net, Want> {
        let (scope, io) = from.scope("net")?;
        let Scope::Net { at, listen } = scope else { return Err(Want::WrongKind { given: scope.kind_name() }) };
        let net = io.need_net().map_err(Want::NotOffered)?;
        let at = match HostPort::parse(at.as_str()) {
            Ok(at) => Addr::Tcp(at),
            Err(_) => unreachable!("a spec's endpoint was checked as it was read"),
        };
        Ok(match listen {
            true => net.listen_only(&at),
            false => net.connect_only(&at),
        })
    }
}

impl Grant for crate::env::Env {
    const KIND: Kind = Kind::Env;

    fn grant(from: &Granting<'_>) -> Result<crate::env::Env, Want> {
        let (scope, io) = from.scope("env")?;
        let Scope::Env { names } = scope else { return Err(Want::WrongKind { given: scope.kind_name() }) };
        let env = io.need_env().map_err(Want::NotOffered)?;
        let mut kept = Vec::with_capacity_in(names.len(), from.alloc());
        for name in names.iter() {
            match crate::env::VarName::new(name.as_str()) {
                Ok(name) => kept.push(name),
                Err(_) => unreachable!("a spec's variable names were checked as it was read"),
            }
        }
        Ok(env.only(&kept))
    }
}

/// The root capabilities a grant hands over whole, each by its name in `Offers::NAMES`.
macro_rules! whole {
    ($($ty:path => $name:literal, $need:ident;)*) => {$(
        impl Grant for $ty {
            const KIND: Kind = Kind::Whole($name);

            fn grant(from: &Granting<'_>) -> Result<$ty, Want> {
                // An ordinary start: the person who typed the command runs it with their own
                // authority, so a whole capability the manifest names is the process's; the
                // manifest's work there is to drop what `main` did not name (coordinator's
                // decision 1, 2026-11-16). A spec start gives only what it grants.
                if from.started() == Started::Ordinary {
                    return from.process().$need().map_err(Want::NotOffered);
                }
                let (scope, io) = from.scope("whole")?;
                match scope {
                    Scope::Whole { capability, device: None } if capability.as_str() == $name => io.$need().map_err(Want::NotOffered),
                    // One device of a capability this type cannot narrow to a device.
                    Scope::Whole { capability, device: Some(_) } if capability.as_str() == $name => {
                        Err(Want::WrongKind { given: "device" })
                    }
                    _ => Err(Want::WrongKind { given: scope.kind_name() }),
                }
            }
        }
    )*};
}

whole! {
    crate::fs::Fs => "fs", need_fs;
    crate::network::Network => "net", need_network;
    crate::rng::Rng => "rng", need_rng;
    crate::host::Host => "host", need_host;
    crate::streams::Streams => "streams", need_streams;
    crate::term::Term => "term", need_term;
    crate::keys::Keys => "keys", need_keys;
    crate::roots::Roots => "roots", need_roots;
    crate::locale::Locale => "locale", need_locale;
    crate::user::User => "user", need_user;
    crate::code::Code => "code", need_code;
    crate::window::Window => "windows", need_windows;
    crate::automation::Automation => "automation", need_automation;
    crate::opener::Opener => "opener", need_opener;
    crate::power::Power => "power", need_power;
    crate::audio::Audio => "audio", need_audio;
    crate::device::Screen => "screen", need_screen;
    crate::device::Location => "location", need_location;
    crate::device::Clipboard => "clipboard", need_clipboard;
    crate::device::Notify => "notify", need_notify;
    crate::gpu::Gpu => "gpu", need_gpu;
}

/// `hermetic: Hermetic` (D177): the one way a program runs another hermetically, a whole
/// capability by machine ([`BY_MACHINE`]), made from the host's spawner and the run's clock and
/// handed on as the narrower authority: it starts a child only under a plan's confinement. A
/// `main` that may start children only this way names `Hermetic` and never `Host`.
#[cfg(unix)]
impl Grant for crate::procs::hermetic::Hermetic {
    const KIND: Kind = Kind::Whole("hermetic");

    fn grant(from: &Granting<'_>) -> Result<crate::procs::hermetic::Hermetic, Want> {
        // As every whole capability (the `whole!` rows): an ordinary start's is the process's;
        // a spec start's is its grant's provider's, and only when granted whole.
        let io = match from.started() {
            Started::Ordinary => from.process(),
            Started::WithSpec => {
                let (scope, io) = from.scope("whole")?;
                match scope {
                    Scope::Whole { capability, device: None } if capability.as_str() == "hermetic" => io,
                    Scope::Whole { capability, device: Some(_) } if capability.as_str() == "hermetic" => {
                        return Err(Want::WrongKind { given: "device" });
                    }
                    _ => return Err(Want::WrongKind { given: scope.kind_name() }),
                }
            }
        };
        let spawner = io.need_host().and_then(|host| host.spawner()).map_err(Want::NotOffered)?;
        let clock = io.need_clock().map_err(Want::NotOffered)?;
        Ok(crate::procs::hermetic::Hermetic::new(crate::procs::hermetic::HermeticCapOpts { spawner, clock }))
    }
}

// Requests, behind bstd's `net` feature.
#[cfg(feature = "requests")]
whole! {
    crate::requests::Http => "http", need_http;
}

/// The launcher's state while it fills `main`'s parameters: the spec, the worlds and the
/// manifest each parameter is made from. [`Given::checking`] (made.rs) checks each parameter by
/// MAKING it, once, and keeps what it made for `main` (rule 74).
pub struct Given<'g> {
    cx: &'g Cx,
    real: &'g Io,
    fake: Option<&'g Io>,
    spec: &'g Spec,
    started: Started,
    manifest: &'static Manifest,
    /// The process's heap the entry handed down: what a `heap: Heap` parameter is (D120).
    heap: crate::mem::Heap,
}

impl<'g> Given<'g> {
    pub fn new(
        cx: &'g Cx,
        real: &'g Io,
        fake: Option<&'g Io>,
        spec: &'g Spec,
        started: Started,
        manifest: &'static Manifest,
        heap: crate::mem::Heap,
    ) -> Given<'g> {
        Given { cx, real, fake, spec, started, manifest, heap }
    }
}

/// What a launcher does before `main`, once it knows what file descriptor 3 held.
pub enum Prepared {
    /// Run `main` with this spec: the fake world made when the spec asks for one, and how many
    /// arguments after the program's name an ordinary start's `--grant`s took (0 with a spec).
    Run { spec: Spec, started: Started, fake: Option<Io>, taken: usize },
    /// Print this manifest and exit [`Status::OK`].
    Manifest(String<LaunchAlloc>),
    /// Exit [`Status::REFUSED`], saying this.
    Refuse(String<LaunchAlloc>),
}

/// The launcher's decision from what file descriptor 3 held (`None`: it was closed), the spec
/// of an ordinary start (`defaults`) and its command line (`args`, the program's name first:
/// its leading `--grant`s, then the attribute's defaults, are an ordinary start's grants,
/// defaults.rs); what it makes is in `alloc`, the entry's (D102).
pub fn prepare(
    manifest: &'static Manifest,
    delivered: Option<&[u8]>,
    defaults: impl FnOnce() -> RunSpec<LaunchAlloc>,
    args: &[&str],
    alloc: LaunchAlloc,
) -> Prepared {
    let mut taken = 0;
    let (spec, started) = match delivered {
        None => {
            let mut spec = defaults();
            match ordinary_grants(manifest, args, &mut spec, alloc) {
                Ok(took) => taken = took,
                Err(why) => {
                    let mut said = String::new_in(alloc);
                    blaze_text::PutDisplay::put_display(&mut said, &why);
                    return Prepared::Refuse(said);
                }
            }
            (spec, Started::Ordinary)
        }
        Some(bytes) => match crate::spec::read_delivery(bytes, alloc) {
            Ok(crate::spec::Asked::Manifest) => return Prepared::Manifest(manifest.write(alloc)),
            Ok(crate::spec::Asked::Run(spec)) => (spec, Started::WithSpec),
            Err(error) => {
                let mut said = String::new_in(alloc);
                blaze_text::PutDisplay::put_display(&mut said, &error);
                return Prepared::Refuse(said);
            }
        },
    };
    let fake = needs_fake(&spec).then(|| {
        let mut world = crate::io::SimWorld::new(spec.seed, crate::io::Limits::USUAL);
        world.env = spec.env.iter().map(|(name, value)| (name.as_str().into(), value.as_str().into())).collect();
        world.args = spec.args.iter().map(|arg| arg.as_str().into()).collect();
        Io::simulated(world)
    });
    Prepared::Run { spec: Spec::new(spec, alloc), started, fake, taken }
}

/// Whether the run or any grant asks for the fake provider.
fn needs_fake(spec: &RunSpec<LaunchAlloc>) -> bool {
    spec.provider == Provider::Fake || spec.grants.iter().any(|granted| granted.provider == Some(Provider::Fake))
}

/// `body`, the macro's filling and call of `main`, as the `main` the entry runs: its root
/// context narrowed to the spec's time limit; `heap` is the entry's, what a `heap: Heap`
/// parameter is handed.
pub fn main_of<'p, R: Outcome>(
    manifest: &'static Manifest,
    spec: &'p Spec,
    started: Started,
    fake: Option<&'p Io>,
    heap: crate::mem::Heap,
    body: impl FnOnce(&mut Given<'_>) -> Result<R, Refused> + 'p,
) -> impl FnOnce(&Cx, Io) -> Launched<R> + 'p {
    move |root: &Cx, io: Io| {
        let limited = spec.get().limits.time_ms.map(|ms| {
            root.until(blaze_core::wait::Deadline::after(io.now(), core::time::Duration::from_millis(ms)))
        });
        let cx = limited.as_ref().unwrap_or(root);
        let mut given = Given::new(cx, &io, fake, spec, started, manifest, heap);
        match body(&mut given) {
            Ok(ran) => Launched::Ran(ran),
            Err(refused) => Launched::Refused(refused),
        }
    }
}
