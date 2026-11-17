//! A deferred C request as a spawn request (READINESS.md 1e, prompt W4), pure.
//!
//! `check` keeps each native tool spawn of a build script as its argv joined by NUL
//! ([`crate::Outputs::c_requests`], layout `bzbsout3`, in spawn order). [`to_spawn`] makes one of
//! those texts a [`SpawnRequest`] for the deterministic sandbox: the program is replaced by the
//! pinned tool (`clang` for `cc`/`clang`/`gcc`, `clang++` for `c++`/`clang++`/`g++`, `llvm-ar`
//! for `ar`; any other C tool only by a pinned tool of its own name), the arguments are kept in
//! order, the package tree (at its mount: `/package` for a dependency, `/workspace` for a
//! member, decision D96) and the SDK are the reads, `/out` is the one
//! writable folder, and the environment is `fixed_env` and nothing else (cc's environment is in
//! its arguments). The text stays the record; the request is derived from it, so the layout does
//! not change, and two derivations of one text give one key.
//!
//! An argument naming a path outside the guest's view (anything absolute not under
//! the tree's mount, `/out` or the sysroot's guest path, or climbing out of one with `..`) is refused
//! by name ([`CRequestError::HostPath`]): the `-ffile-prefix-map` rule of build-scripts.md "The
//! record" should have mapped it already, and a host path in a key would make the key the
//! machine's (rule 25).

#![blaze::sans_io]

use blaze_expand::seen::SeenRoot;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_spawn::{Input, MACHINE_V1, Personality, RequestError, SpawnRequest, Tool};

use crate::probe::is_c_tool;
/// The guest path of `OUT_DIR`, the one writable folder.
pub const OUT: &str = "/out";

/// What a C request is made a spawn request against: the step and the pinned tools.
pub struct CContext<'c> {
    /// The step's key: it seeds the guest's clock and randomness.
    pub step: ContentId,
    /// The pinned `clang` (the program for `cc`, `clang`, `gcc`).
    pub clang: Tool<'c>,
    /// Every other pinned tool (`clang++`, `llvm-ar`, and what the program may start), by name.
    pub tools: &'c [Tool<'c>],
    /// What the script sees as its root: the package tree is served at its `mount()` (D96).
    pub seen: SeenRoot<'c>,
    /// The package's source tree (a member's: its workspace), served at `seen.mount()`.
    pub package_tree: ContentId,
    /// The SDK, at its guest path.
    pub sysroot: Input<'c>,
    /// The files already in [`OUT`], by guest path, with their digests.
    pub out_present: &'c [(&'c str, ContentId)],
    /// The guest working directory the script spawned the tool in.
    pub cwd: &'c str,
    /// The identity of the guest's userland (dyld, shared cache, libSystem): W1's
    /// `SpawnRequest::userland`. NOT in the W4 prompt's text: added because `SpawnRequest::new`
    /// takes it and nothing else here can supply it (a made-up value would be a key missing an
    /// input, rule 21).
    pub userland: ContentId,
}

/// One argument of the request text, by place: its index in the argv (0 is the program) and
/// its byte range in the text, so an error names it without owning a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgAt {
    pub index: u32,
    pub start: u32,
    pub end: u32,
}

impl ArgAt {
    /// The argument's text in the request text it came from; `None` if `argv_nul` is another text.
    pub fn text<'t>(&self, argv_nul: &'t str) -> Option<&'t str> {
        argv_nul.get(self.start as usize..self.end as usize)
    }
}

/// The rule of `SpawnRequest::new` (W1) that refused the request made here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestRule {
    EnvTwice,
    FixedEnv,
    ReadTwice,
    WriteTwice,
    ToolTwice,
    PresentTwice,
    Path,
}

/// Why a C request is not made a spawn request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CRequestError {
    /// The request text holds no program.
    Empty,
    /// The program is not a C toolchain program (`probe::is_c_tool`).
    NotCTool { program: ArgAt },
    /// A C toolchain program with no pinned tool for it in the context.
    ToolNotPinned { program: ArgAt },
    /// An argument naming a path outside the tree's mount, `/out` and the sysroot's guest path.
    HostPath { arg: ArgAt },
    /// The spawn request itself was refused by `SpawnRequest::new`.
    Request(RequestRule),
}

impl CRequestError {
    /// The error with the text it is about, for printing.
    pub fn show<'e>(&'e self, argv_nul: &'e str) -> Shown<'e> {
        Shown { error: self, argv_nul }
    }
}

/// A [`CRequestError`] printed with its request text.
pub struct Shown<'e> {
    error: &'e CRequestError,
    argv_nul: &'e str,
}

impl core::fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = |at: &ArgAt| at.text(self.argv_nul).unwrap_or("<argument of another request>");
        match self.error {
            CRequestError::Empty => f.write_str("a deferred C request holds no program (W4)"),
            CRequestError::NotCTool { program } => {
                write!(f, "`{}` is not a C toolchain program; only those are made C requests (W4)", text(program))
            }
            CRequestError::ToolNotPinned { program } => {
                write!(f, "`{}` has no pinned tool to run in the deterministic sandbox (W4, D92)", text(program))
            }
            CRequestError::HostPath { arg } => write!(
                f,
                "argument {} `{}` names a path outside the package tree's mount, /out and the sysroot: a host path in a spawn \
                 request makes its key the machine's (rule 25; the -ffile-prefix-map rule should have mapped it)",
                arg.index,
                text(arg)
            ),
            CRequestError::Request(rule) => write!(f, "the spawn request was refused by its rule {rule:?} (W1)"),
        }
    }
}

/// The pinned tool's name for a C toolchain program's name.
fn pinned_name(name: &str) -> &str {
    match name {
        "cc" | "clang" | "gcc" => "clang",
        "c++" | "clang++" | "g++" => "clang++",
        "ar" | "llvm-ar" => "llvm-ar",
        other => other,
    }
}

fn own<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    String::from_str_in(text, alloc)
}

fn own_tool<'a>(tool: &Tool<'_>, alloc: Alloc<'a>) -> Tool<'a> {
    Tool { name: own(tool.name.as_str(), alloc), digest: tool.digest, guest_path: own(tool.guest_path.as_str(), alloc) }
}

fn own_input<'a>(input: &Input<'_>, alloc: Alloc<'a>) -> Input<'a> {
    match input {
        Input::File { path, digest } => Input::File { path: own(path.as_str(), alloc), digest: *digest },
        Input::Tree { path, digest } => Input::Tree { path: own(path.as_str(), alloc), digest: *digest },
    }
}

fn input_path<'i>(input: &'i Input<'_>) -> &'i str {
    match input {
        Input::File { path, .. } | Input::Tree { path, .. } => path.as_str(),
    }
}

/// Flags that take a path joined to them (`-I/x`); the path is checked like a whole argument.
const JOINED: &[&str] = &[
    "-isystem", "-iquote", "-idirafter", "-iprefix", "-iwithprefix", "-include", "-imacros", "-isysroot", "-MF",
    "-MT", "-MQ", "-I", "-L", "-F", "-B", "-o",
];

/// Whether `path` (absolute) is `root` or under it, and climbs nowhere with `..`.
fn under(path: &str, root: &str) -> bool {
    let inside = path == root || path.strip_prefix(root).is_some_and(|rest| rest.starts_with('/'));
    inside && !path.split('/').any(|part| part == "..")
}

/// Whether every absolute path in `arg` is inside the guest's view: the whole argument, the
/// path joined to a [`JOINED`] flag, and each `=`-separated part (`--sysroot=/x`,
/// `-ffile-prefix-map=/a=/b`).
fn in_view(arg: &str, mount: &str, sysroot: &str) -> bool {
    let ok = |path: &str| !path.starts_with('/') || under(path, mount) || under(path, OUT) || under(path, sysroot);
    if !ok(arg) {
        return false;
    }
    if arg.starts_with('-') {
        if let Some(rest) = JOINED.iter().find_map(|flag| arg.strip_prefix(flag)) {
            if !ok(rest) {
                return false;
            }
        }
        if arg.split('=').skip(1).any(|part| !ok(part)) {
            return false;
        }
    }
    true
}

fn rule(error: &RequestError) -> RequestRule {
    match error {
        RequestError::EnvTwice { .. } => RequestRule::EnvTwice,
        RequestError::FixedEnv { .. } => RequestRule::FixedEnv,
        RequestError::ReadTwice { .. } => RequestRule::ReadTwice,
        RequestError::WriteTwice { .. } => RequestRule::WriteTwice,
        RequestError::ToolTwice { .. } => RequestRule::ToolTwice,
        RequestError::PresentTwice { .. } => RequestRule::PresentTwice,
        RequestError::Path { .. } => RequestRule::Path,
    }
}

/// Makes one `Outputs::c_requests` entry (argv joined by NUL) a spawn request for the
/// deterministic sandbox. Pure: the same text and context give the same request, so one key.
pub fn to_spawn<'a>(argv_nul: &str, at: &CContext<'_>, alloc: Alloc<'a>) -> Result<SpawnRequest<'a>, CRequestError> {
    let mut start = 0usize;
    let mut program: Option<(ArgAt, &str)> = None;
    let mut args: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let sysroot = input_path(&at.sysroot);
    for (index, text) in argv_nul.split('\0').enumerate() {
        let place = ArgAt { index: index as u32, start: start as u32, end: (start + text.len()) as u32 };
        start += text.len() + 1;
        if index == 0 {
            if text.is_empty() {
                return Err(CRequestError::Empty);
            }
            program = Some((place, text));
            continue;
        }
        if !in_view(text, at.seen.mount(), sysroot) {
            return Err(CRequestError::HostPath { arg: place });
        }
        args.push(own(text, alloc));
    }
    let Some((place, text)) = program else { return Err(CRequestError::Empty) };
    if !is_c_tool(text) {
        return Err(CRequestError::NotCTool { program: place });
    }
    let name = pinned_name(text.rsplit('/').next().unwrap_or(text));
    let pinned = if name == "clang" {
        &at.clang
    } else {
        at.tools.iter().find(|tool| tool.name.as_str() == name).ok_or(CRequestError::ToolNotPinned { program: place })?
    };
    let mut tools: AVec<'a, Tool<'a>> = Vec::new_in(alloc);
    at.tools.iter().for_each(|tool| tools.push(own_tool(tool, alloc)));
    let mut reads: AVec<'a, Input<'a>> = Vec::new_in(alloc);
    reads.push(Input::Tree { path: own(at.seen.mount(), alloc), digest: at.package_tree });
    reads.push(own_input(&at.sysroot, alloc));
    let mut writes: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    writes.push(own(OUT, alloc));
    let mut present: AVec<'a, (AString<'a>, ContentId)> = Vec::new_in(alloc);
    at.out_present.iter().for_each(|(path, digest)| present.push((own(path, alloc), *digest)));
    SpawnRequest::new(
        at.step,
        Personality::Xnu,
        MACHINE_V1,
        at.userland,
        own_tool(pinned, alloc),
        tools,
        args,
        Vec::new_in(alloc),
        own(at.cwd, alloc),
        Vec::new_in(alloc),
        reads,
        writes,
        present,
    )
    .map_err(|error| CRequestError::Request(rule(&error)))
}
