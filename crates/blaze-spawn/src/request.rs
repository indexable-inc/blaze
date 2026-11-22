//! `SpawnRequest`: a spawned command as a canonical record keyed from text (READINESS.md 1a, S1;
//! 1d's time zone, locale and `SOURCE_DATE_EPOCH` rows).

use core::fmt;

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc};

use crate::{Malformed, Put, Take, guest_path_ok};

/// The request record's layout word.
const LAYOUT: u64 = u64::from_le_bytes(*b"bzspreq1");

/// The key's domain.
const DOMAIN: &str = "blaze v2 spawn request 1";

/// The kernel interface the guest program is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Personality {
    Xnu = 1,
    Linux = 2,
}

impl Personality {
    fn from_tag(tag: u8) -> Option<Personality> {
        match tag {
            1 => Some(Personality::Xnu),
            2 => Some(Personality::Linux),
            _ => None,
        }
    }
}

/// The machine the guest is told it runs on, whatever the host is: part of the key, so a tool that
/// sizes its work by the core count gives the same bytes everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Machine {
    pub cpus: u16,
    pub page: u32,
    pub memory: u64,
}

/// The one declared machine today.
pub const MACHINE_V1: Machine = Machine { cpus: 1, page: 16384, memory: 8 << 30 };

/// A program by its pinned bytes' SHA-256 and where the guest sees it; never a host path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool<'a> {
    pub name: AString<'a>,
    pub digest: [u8; 32],
    pub guest_path: AString<'a>,
}

/// One entry of the read grant, by content. A folder whole (`Tree`) is a coarse proof covering
/// all under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input<'a> {
    File { path: AString<'a>, digest: ContentId },
    Tree { path: AString<'a>, digest: ContentId },
}

impl Input<'_> {
    /// The guest path the entry grants.
    pub fn path(&self) -> &str {
        match self {
            Input::File { path, .. } | Input::Tree { path, .. } => path.as_str(),
        }
    }

    fn tag(&self) -> u8 {
        match self {
            Input::File { .. } => 1,
            Input::Tree { .. } => 2,
        }
    }

    fn digest(&self) -> &ContentId {
        match self {
            Input::File { digest, .. } | Input::Tree { digest, .. } => digest,
        }
    }
}

/// The environment every request holds, sorted by name: the C locale, the epoch as the build
/// date, and UTC as the time zone (READINESS.md 1d). A request may restate one with the same
/// value; another value is refused.
pub fn fixed_env() -> [(&'static str, &'static str); 4] {
    [("LANG", "C"), ("LC_ALL", "C"), ("SOURCE_DATE_EPOCH", "0"), ("TZ", "UTC0")]
}

/// A spawned command as a record. Field order is the encoding's order and the key's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRequest<'a> {
    /// The step's key: seeds the guest's clock and randomness.
    pub step: ContentId,
    pub personality: Personality,
    pub machine: Machine,
    /// The identity of the guest's dyld, shared cache and libSystem (the host's OS build digest
    /// on macOS).
    pub userland: ContentId,
    pub program: Tool<'a>,
    /// Further programs the program may start (nested spawns), sorted by name.
    pub tools: AVec<'a, Tool<'a>>,
    pub args: AVec<'a, AString<'a>>,
    /// The whole environment, sorted by name, the fixed entries among it.
    pub env: AVec<'a, (AString<'a>, AString<'a>)>,
    pub cwd: AString<'a>,
    pub stdin: AVec<'a, u8>,
    /// The read grant, sorted by path.
    pub reads: AVec<'a, Input<'a>>,
    /// The writable folders, sorted.
    pub writes: AVec<'a, AString<'a>>,
    /// The files already in the writable folders, with their digests, sorted by path.
    pub present: AVec<'a, (AString<'a>, ContentId)>,
}

/// Why a request was not made. Each names the entry it refused. (The planner wrote the type
/// without a lifetime; it carries the refused entry's own string, so it has the request's.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError<'a> {
    /// The environment names one variable twice.
    EnvTwice { name: AString<'a> },
    /// The environment sets a fixed variable (`fixed_env`) to another value.
    FixedEnv { name: &'static str },
    /// The read grant names one path twice.
    ReadTwice { path: AString<'a> },
    /// The writable folders name one path twice.
    WriteTwice { path: AString<'a> },
    /// Two tools have one name.
    ToolTwice { name: AString<'a> },
    /// Two present files have one path.
    PresentTwice { path: AString<'a> },
    /// A guest path is not absolute, or holds a `..` component or a NUL.
    Path { what: &'static str, path: AString<'a> },
}

impl fmt::Display for RequestError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::EnvTwice { name } => {
                write!(f, "`{name}`: a spawn request's environment names it twice (one value per name)")
            }
            RequestError::FixedEnv { name } => write!(
                f,
                "`{name}`: a spawn request may not change it; every spawn runs with it fixed so its output is the same on every machine (READINESS.md 1d)"
            ),
            RequestError::ReadTwice { path } => write!(f, "`{path}`: a spawn request's read grant names it twice"),
            RequestError::WriteTwice { path } => {
                write!(f, "`{path}`: a spawn request's writable folders name it twice")
            }
            RequestError::ToolTwice { name } => write!(f, "`{name}`: a spawn request pins two tools by this name"),
            RequestError::PresentTwice { path } => {
                write!(f, "`{path}`: a spawn request's present files name it twice")
            }
            RequestError::Path { what, path } => write!(
                f,
                "`{path}`: a spawn request's {what} must be an absolute guest path without `..` or NUL"
            ),
        }
    }
}

impl core::error::Error for RequestError<'_> {}

/// Sorts `items` by `key` and moves out the first entry whose key repeats.
fn sort_once<T, K: Ord + ?Sized>(items: &mut AVec<'_, T>, key: impl Fn(&T) -> &K) -> Option<T> {
    items.sort_unstable_by(|a, b| key(a).cmp(key(b)));
    let twice = items.windows(2).position(|pair| key(&pair[0]) == key(&pair[1]))?;
    Some(items.remove(twice + 1))
}

/// Whether `items` is strictly increasing by `key` (so sorted, and each key once).
fn strictly_sorted<T, K: Ord + ?Sized>(items: &[T], key: impl Fn(&T) -> &K) -> bool {
    items.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}

impl<'a> SpawnRequest<'a> {
    /// A canonical request: the environment sorted with the fixed entries inserted, every set
    /// sorted, and every repeat, changed fixed variable and ill-formed guest path refused.
    pub fn new(
        step: ContentId,
        personality: Personality,
        machine: Machine,
        userland: ContentId,
        program: Tool<'a>,
        mut tools: AVec<'a, Tool<'a>>,
        args: AVec<'a, AString<'a>>,
        mut env: AVec<'a, (AString<'a>, AString<'a>)>,
        cwd: AString<'a>,
        stdin: AVec<'a, u8>,
        mut reads: AVec<'a, Input<'a>>,
        mut writes: AVec<'a, AString<'a>>,
        mut present: AVec<'a, (AString<'a>, ContentId)>,
    ) -> Result<SpawnRequest<'a>, RequestError<'a>> {
        if let Some((name, _)) = sort_once(&mut env, |(name, _)| name.as_str()) {
            return Err(RequestError::EnvTwice { name });
        }
        let alloc = *env.allocator();
        for (name, value) in fixed_env() {
            match env.binary_search_by(|(have, _)| have.as_str().cmp(name)) {
                Ok(at) => {
                    if env[at].1.as_str() != value {
                        return Err(RequestError::FixedEnv { name });
                    }
                }
                Err(at) => env.insert(at, (AString::from_str_in(name, alloc), AString::from_str_in(value, alloc))),
            }
        }
        if let Some(tool) = sort_once(&mut tools, |tool| tool.name.as_str()) {
            return Err(RequestError::ToolTwice { name: tool.name });
        }
        if let Some(input) = sort_once(&mut reads, |input| input.path()) {
            let path = match input {
                Input::File { path, .. } | Input::Tree { path, .. } => path,
            };
            return Err(RequestError::ReadTwice { path });
        }
        if let Some(path) = sort_once(&mut writes, |path| path.as_str()) {
            return Err(RequestError::WriteTwice { path });
        }
        if let Some((path, _)) = sort_once(&mut present, |(path, _)| path.as_str()) {
            return Err(RequestError::PresentTwice { path });
        }
        let request =
            SpawnRequest { step, personality, machine, userland, program, tools, args, env, cwd, stdin, reads, writes, present };
        if let Some(what) = request.ill_formed_path() {
            return Err(request.refuse_path(what));
        }
        Ok(request)
    }

    /// The first field holding a guest path that is not absolute or holds `..` or a NUL.
    fn ill_formed_path(&self) -> Option<&'static str> {
        if !guest_path_ok(&self.cwd) {
            return Some("cwd");
        }
        if !guest_path_ok(&self.program.guest_path) {
            return Some("program");
        }
        if !self.tools.iter().all(|tool| guest_path_ok(&tool.guest_path)) {
            return Some("tools");
        }
        if !self.reads.iter().all(|input| guest_path_ok(input.path())) {
            return Some("reads");
        }
        if !self.writes.iter().all(|path| guest_path_ok(path)) {
            return Some("writes");
        }
        if !self.present.iter().all(|(path, _)| guest_path_ok(path)) {
            return Some("present");
        }
        None
    }

    /// The `Path` refusal for the first ill-formed path of field `what`, moved out of `self`.
    fn refuse_path(self, what: &'static str) -> RequestError<'a> {
        let path = match what {
            "cwd" => Some(self.cwd),
            "program" => Some(self.program.guest_path),
            "tools" => self.tools.into_iter().map(|tool| tool.guest_path).find(|path| !guest_path_ok(path)),
            "reads" => self
                .reads
                .into_iter()
                .map(|input| match input {
                    Input::File { path, .. } | Input::Tree { path, .. } => path,
                })
                .find(|path| !guest_path_ok(path)),
            "writes" => self.writes.into_iter().find(|path| !guest_path_ok(path)),
            _ => self.present.into_iter().map(|(path, _)| path).find(|path| !guest_path_ok(path)),
        };
        let Some(path) = path else {
            unreachable!("blaze-spawn: `{what}` was found ill-formed and then holds no ill-formed path");
        };
        RequestError::Path { what, path }
    }

    /// The one encoding: the layout word, then every field in declared order.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        let mut put = Put { out };
        put.u64(LAYOUT);
        put.id(&self.step);
        put.u8(self.personality as u8);
        put.u16(self.machine.cpus);
        put.u32(self.machine.page);
        put.u64(self.machine.memory);
        put.id(&self.userland);
        put_tool(&mut put, &self.program);
        put.count(self.tools.len(), "tools");
        for tool in self.tools.iter() {
            put_tool(&mut put, tool);
        }
        put.count(self.args.len(), "args");
        for arg in self.args.iter() {
            put.str(arg, "args");
        }
        put.count(self.env.len(), "env");
        for (name, value) in self.env.iter() {
            put.str(name, "env");
            put.str(value, "env");
        }
        put.str(&self.cwd, "cwd");
        put.bytes(&self.stdin, "stdin");
        put.count(self.reads.len(), "reads");
        for input in self.reads.iter() {
            put.u8(input.tag());
            put.str(input.path(), "reads");
            put.id(input.digest());
        }
        put.count(self.writes.len(), "writes");
        for path in self.writes.iter() {
            put.str(path, "writes");
        }
        put.count(self.present.len(), "present");
        for (path, digest) in self.present.iter() {
            put.str(path, "present");
            put.id(digest);
        }
    }

    /// The request's key: its encoding under the domain `"blaze v2 spawn request 1"`.
    pub fn key(&self, alloc: Alloc<'_>) -> ContentId {
        let mut encoded = AVec::new_in(alloc);
        self.encode(&mut encoded);
        ContentId::of(DOMAIN, &encoded)
    }

    /// The request in `bytes`, refused unless canonical: every set strictly sorted, the fixed
    /// environment present with its values, every guest path well formed, nothing after the end.
    pub fn decode<'b>(bytes: &'b [u8], alloc: Alloc<'b>) -> Result<SpawnRequest<'b>, Malformed> {
        let mut take = Take::new(bytes);
        let layout = take.u64()?;
        if layout != LAYOUT {
            return Err(Malformed::Layout(layout));
        }
        let step = take.id()?;
        let Some(personality) = Personality::from_tag(take.u8()?) else {
            return Err(Malformed::NotCanonical { field: "personality" });
        };
        let machine = Machine { cpus: take.u16()?, page: take.u32()?, memory: take.u64()? };
        let userland = take.id()?;
        let program = take_tool(&mut take, "program", alloc)?;
        let mut tools = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            tools.push(take_tool(&mut take, "tools", alloc)?);
        }
        let mut args = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            args.push(take.string("args", alloc)?);
        }
        let mut env = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            let name = take.string("env", alloc)?;
            let value = take.string("env", alloc)?;
            env.push((name, value));
        }
        let cwd = take.string("cwd", alloc)?;
        let stdin = take.byte_vec(alloc)?;
        let mut reads = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            let tag = take.u8()?;
            let path = take.string("reads", alloc)?;
            let digest = take.id()?;
            reads.push(match tag {
                1 => Input::File { path, digest },
                2 => Input::Tree { path, digest },
                _ => return Err(Malformed::NotCanonical { field: "reads" }),
            });
        }
        let mut writes = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            writes.push(take.string("writes", alloc)?);
        }
        let mut present = AVec::new_in(alloc);
        for _ in 0..take.count()? {
            let path = take.string("present", alloc)?;
            let digest = take.id()?;
            present.push((path, digest));
        }
        take.end()?;

        if !strictly_sorted(&tools, |tool| tool.name.as_str()) {
            return Err(Malformed::Unsorted { field: "tools" });
        }
        if !strictly_sorted(&env, |(name, _)| name.as_str()) {
            return Err(Malformed::Unsorted { field: "env" });
        }
        if !strictly_sorted(&reads, |input| input.path()) {
            return Err(Malformed::Unsorted { field: "reads" });
        }
        if !strictly_sorted(&writes, |path| path.as_str()) {
            return Err(Malformed::Unsorted { field: "writes" });
        }
        if !strictly_sorted(&present, |(path, _)| path.as_str()) {
            return Err(Malformed::Unsorted { field: "present" });
        }
        let fixed_held = fixed_env().iter().all(|&(name, value)| {
            env.binary_search_by(|(have, _)| have.as_str().cmp(name)).is_ok_and(|at| env[at].1.as_str() == value)
        });
        if !fixed_held {
            return Err(Malformed::NotCanonical { field: "env" });
        }
        let request =
            SpawnRequest { step, personality, machine, userland, program, tools, args, env, cwd, stdin, reads, writes, present };
        if let Some(field) = request.ill_formed_path() {
            return Err(Malformed::NotCanonical { field });
        }
        Ok(request)
    }
}

fn put_tool(put: &mut Put<'_, '_>, tool: &Tool<'_>) {
    put.str(&tool.name, "tool name");
    put.raw32(&tool.digest);
    put.str(&tool.guest_path, "tool guest path");
}

fn take_tool<'b>(take: &mut Take<'b>, field: &'static str, alloc: Alloc<'b>) -> Result<Tool<'b>, Malformed> {
    let name = take.string(field, alloc)?;
    let digest = take.array::<32>()?;
    let guest_path = take.string(field, alloc)?;
    Ok(Tool { name, digest, guest_path })
}
