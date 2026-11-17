//! A build script's spawn routed in one fixed order decided from text (READINESS.md 1a, S3):
//! `rustc` from the probe table, `git` as a derived fact (D54), a derived fact of
//! `blaze_build_script::facts` (pkg-config, `sw_vers`, a compiler's identity), a deferred C or
//! native tool in `CMode::Deferred`, and everything else to the deterministic sandbox's
//! executor (`blaze_spawn::Executor`), or refused by name when the sandbox is off (D92, D94).
//! No route is tried after another fails (rule 3): the first route that claims a spawn answers
//! it or stops the script.
//!
//! The executor's answer is a function of the request's key, so the script's read list holds
//! `spawn:<request key hex>` with the answer's digest; the files the answer wrote land in the
//! in-memory `OUT_DIR` through the same limits a script's own writes take.

use std::sync::Arc;

use blaze_build_script::facts::{self, Fact};
use blaze_build_script::key::CMode;
use blaze_build_script::probe;
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_spawn::{Executor, Files, Input, MACHINE_V1, NoSandbox, Personality, SpawnAnswer, SpawnRefused, SpawnRequest, Status, Tool, Written};
use blaze_wasi::Stop;

use crate::script_sandbox::{Malformed, Refusal, STDERR_TAIL, Script, ScriptSandbox, SpawnRefusal, Tree, WORKSPACE, git_words};

/// One spawn as the shim encoded it, whole: what `decode` used to read past (the environment
/// and the two output modes) is kept, because the executor's request holds the child's whole
/// environment and the modes decide where its output goes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Decoded {
    pub argv: Vec<String>,
    pub env_clear: bool,
    /// `(set, name, value)` in the order the script made them: `set` false is an unset.
    pub env_ops: Vec<(bool, String, String)>,
    pub cwd: Option<String>,
    pub stdin: Vec<u8>,
    /// `[stdout, stderr]`: 0 inherit, 1 null, 2 pipe, 3 a file, 4 the parent's stdout, 5 the
    /// parent's stderr (guest/script_process.rs).
    pub modes: [u8; 2],
}

/// The shim's request (guest/script_process.rs): argc, argv, env_clear u8, env count, (u8 set,
/// key, value) each, cwd (u8 has, string, empty without), stdin, stdout mode u8, stderr mode u8;
/// numbers u32 LE, strings length-first.
pub(crate) fn decode_full(bytes: &[u8]) -> Result<Decoded, Malformed> {
    struct Cursor<'b> {
        bytes: &'b [u8],
        at: usize,
    }
    impl<'b> Cursor<'b> {
        fn take(&mut self, n: usize) -> Result<&'b [u8], Malformed> {
            let at = self.at;
            let end = at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(Malformed::EndsAt(at))?;
            self.at = end;
            Ok(&self.bytes[at..end])
        }
        fn u32(&mut self) -> Result<u32, Malformed> {
            let b = self.take(4)?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }
        fn u8(&mut self) -> Result<u8, Malformed> {
            Ok(self.take(1)?[0])
        }
        fn string(&mut self) -> Result<&'b [u8], Malformed> {
            let len = self.u32()? as usize;
            self.take(len)
        }
        fn text(&mut self, what: &'static str) -> Result<String, Malformed> {
            let bytes = self.string()?;
            String::from_utf8(bytes.to_vec()).map_err(|_| Malformed::NotUtf8(what))
        }
    }
    let mut c = Cursor { bytes, at: 0 };
    let argc = c.u32()?;
    if argc > 1 << 16 {
        return Err(Malformed::Arguments(argc));
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        argv.push(c.text("an argument")?);
    }
    let env_clear = c.u8()? != 0;
    let mut env_ops = Vec::new();
    // Each operation takes at least nine bytes, so a count past the request's length ends at
    // its end (`EndsAt`) rather than growing the list without bound.
    for _ in 0..c.u32()? {
        let set = c.u8()? != 0;
        let name = c.text("an environment name")?;
        let value = c.text("an environment value")?;
        env_ops.push((set, name, value));
    }
    let has_cwd = c.u8()? != 0;
    let cwd = c.text("the folder")?;
    let stdin = c.string()?.to_vec();
    let modes = c.take(2)?;
    Ok(Decoded { argv, env_clear, env_ops, cwd: has_cwd.then_some(cwd), stdin, modes: [modes[0], modes[1]] })
}

/// The child's environment by std's `Command` semantics: the script's own unless `env_clear`,
/// then each operation in order (a set inserts or replaces, an unset removes); sorted by name.
pub(crate) fn child_env(script_env: &[(String, String)], d: &Decoded) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = if d.env_clear { Vec::new() } else { script_env.to_vec() };
    env.sort_by(|a, b| a.0.cmp(&b.0));
    // A name given twice in the script's list keeps its last value, as a later `env` call does.
    env.dedup_by(|later, earlier| {
        let same = later.0 == earlier.0;
        if same {
            core::mem::swap(&mut earlier.1, &mut later.1);
        }
        same
    });
    for (set, name, value) in &d.env_ops {
        match (env.binary_search_by(|(have, _)| have.as_str().cmp(name.as_str())), *set) {
            (Ok(at), true) => env[at].1.clone_from(value),
            (Err(at), true) => env.insert(at, (name.clone(), value.clone())),
            (Ok(at), false) => {
                env.remove(at);
            }
            (Err(_), false) => {}
        }
    }
    env
}

/// Which answerer takes a spawn ([`route`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Route<'s> {
    /// `rustc`: the probe table.
    Rustc,
    /// `git ...`, or a shell's `-c "git ..."`: a derived fact (D54), or a refusal naming the
    /// git command when it has quoting no row reads.
    Git,
    /// A fact derived from text and pins, needing no sandbox.
    Fact(Fact<'s>),
    /// A C or native tool in `check`: kept as a C request, answered exit 0.
    Deferred,
    /// The deterministic sandbox's executor.
    Executor,
    /// No answerer: the sandbox is off on this machine.
    Refused,
}

/// The route of the spawn of `name` (the program's last path component) with `args`, in this
/// fixed order: `rustc`; a git question; a derived fact; a deferred tool in `CMode::Deferred`;
/// then the executor when there is one, else a refusal. Decided from text alone.
pub(crate) fn route<'s>(name: &'s str, args: &'s [&'s str], c_mode: CMode, has_executor: bool) -> Route<'s> {
    if name == "rustc" {
        return Route::Rustc;
    }
    let shell_git = matches!(name, "sh" | "bash") && matches!(args, ["-c", command] if command.starts_with("git "));
    if shell_git || git_words(name, args).is_some() {
        return Route::Git;
    }
    if let Some(fact) = facts::question(name, args) {
        return Route::Fact(fact);
    }
    if probe::is_deferred_tool(name, args) && c_mode == CMode::Deferred {
        return Route::Deferred;
    }
    if has_executor { Route::Executor } else { Route::Refused }
}

/// A pinned program a spawn may name: its bytes' SHA-256 and where the guest sees it (never a
/// host path).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PinnedTool {
    pub name: String,
    pub digest: [u8; 32],
    pub guest_path: String,
}

/// The pins the derived facts answer from (`blaze_build_script::facts::Pins`, owned).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FactPins {
    pub clang_version_text: String,
    pub macos_product_version: String,
    pub macos_build_version: String,
}

/// What the command's setting says about spawns, chosen once per command beside its executor
/// ([`Script::spawner`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpawnSetting {
    /// Whether the owner allowed hvbox to run (D94): only says which refusal a spawn with no
    /// executor gets; it never starts anything.
    pub hvbox_allowed: bool,
    /// The pinned tools, by name.
    pub tools: Vec<PinnedTool>,
    /// The guest userland's identity (on macOS the host's OS build digest); `None` when the
    /// command has none.
    pub userland: Option<ContentId>,
    /// The derived facts' pins; `None` refuses a fact that needs them by name.
    pub pins: Option<FactPins>,
}

impl SpawnSetting {
    /// The sandbox off: hvbox not allowed, no tool pinned, no userland, no pins.
    pub fn off() -> SpawnSetting {
        SpawnSetting { hvbox_allowed: false, tools: Vec::new(), userland: None, pins: None }
    }

    /// Why a spawn finds no sandbox: the owner's word first, then the missing pin.
    pub fn no_sandbox(&self) -> NoSandbox {
        if self.hvbox_allowed { NoSandbox::ToolNotPinned } else { NoSandbox::OwnerHasNotAllowed }
    }
}

impl core::fmt::Debug for Script {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Script")
            .field("package", &self.package)
            .field("tree", &self.tree)
            .field("folder", &self.folder)
            .field("env", &self.env)
            .field("target", &self.target)
            .field("host", &self.host)
            .field("c_mode", &self.c_mode)
            .field("limits", &self.limits)
            .field("table", &self.table)
            .field("spawner", &self.spawner.as_ref().map(|executor| executor.name()))
            .field("step", &self.step)
            .field("spawn_setting", &self.spawn_setting)
            .finish()
    }
}

/// The digest of a reply (code, stdout, stderr), for a fact's read.
pub(crate) fn reply_digest(code: i32, stdout: &[u8], stderr: &[u8]) -> ContentId {
    let mut bytes = Vec::with_capacity(12 + stdout.len() + stderr.len());
    bytes.extend_from_slice(&code.to_le_bytes());
    for part in [stdout, stderr] {
        bytes.extend_from_slice(&(part.len() as u32).to_le_bytes());
        bytes.extend_from_slice(part);
    }
    ContentId::of("blaze build-script spawn reply 1", &bytes)
}

/// A file's digest in a spawn request (`present`).
pub(crate) fn file_digest(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze v2 spawn file 1", bytes)
}

/// The served tree's digest, the coarse proof covering every file under `/workspace`: each
/// file's path and its bytes' digest, in path order (the tree's files are sorted by path).
pub(crate) fn tree_digest(files: &[(String, Vec<u8>)]) -> ContentId {
    let mut bytes = Vec::with_capacity(files.len() * 48);
    for (path, file) in files {
        bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.extend_from_slice(&file_digest(file).0);
    }
    ContentId::of("blaze v2 spawn tree 1", &bytes)
}

/// 64 lowercase hex digits.
pub(crate) fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    out
}

/// The bytes behind a request's reads and present files: the served tree at `/workspace` and
/// the in-memory `OUT_DIR`, by guest path.
struct SpawnFiles<'s> {
    tree: &'s [(String, Vec<u8>)],
    sandbox: &'s ScriptSandbox,
    out_dir: &'s str,
}

impl Files for SpawnFiles<'_> {
    fn bytes(&self, path: &str) -> Option<&[u8]> {
        if let Some(rel) = path.strip_prefix(WORKSPACE).and_then(|rest| rest.strip_prefix('/')) {
            let at = self.tree.binary_search_by(|(have, _)| have.as_str().cmp(rel)).ok()?;
            return Some(self.tree[at].1.as_slice());
        }
        let rel = path.strip_prefix(self.out_dir).and_then(|rest| rest.strip_prefix('/'))?;
        self.sandbox.out.get(rel).and_then(|held| held.as_deref())
    }
}

impl ScriptSandbox {
    /// The reply the shim reads: code i32 LE, then stdout and stderr each u32 length-first.
    pub(crate) fn set_reply(&mut self, code: i32, stdout: &[u8], stderr: &[u8]) {
        self.reply.clear();
        self.reply.extend_from_slice(&code.to_le_bytes());
        for bytes in [stdout, stderr] {
            self.reply.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            self.reply.extend_from_slice(bytes);
        }
    }

    fn stderr_push(&mut self, bytes: &[u8]) {
        self.stderr.extend_from_slice(bytes);
        if self.stderr.len() > STDERR_TAIL {
            self.stderr.drain(..self.stderr.len() - STDERR_TAIL);
        }
    }

    /// A child's outputs sent where its modes say: inherited (0) or the parent's stdout (4)
    /// stdout joins the script's own through the line limit (a child's `cargo:` lines count
    /// as the script's, as under cargo); a pipe (2) goes back in the reply; null (1) is
    /// dropped; a file (3) is refused by name.
    fn deliver(&mut self, program: &str, modes: [u8; 2], code: i32, stdout: &[u8], stderr: &[u8]) -> Result<(), Stop> {
        let mut reply_out: &[u8] = &[];
        let mut reply_err: &[u8] = &[];
        for (stream, (mode, bytes)) in ["stdout", "stderr"].into_iter().zip([(modes[0], stdout), (modes[1], stderr)]) {
            match (stream, mode) {
                (_, 1) => {}
                ("stdout", 2) => reply_out = bytes,
                (_, 2) => reply_err = bytes,
                ("stdout", 0) | (_, 4) => self.stdout_write(bytes)?,
                (_, 0) | (_, 5) => self.stderr_push(bytes),
                _ => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Mode { program: program.to_owned(), stream, mode }))),
            }
        }
        self.set_reply(code, reply_out, reply_err);
        Ok(())
    }

    /// A derived fact's answer (W5), recorded as its subject with the reply's digest.
    pub(crate) fn spawn_fact(&mut self, fact: &Fact<'_>, program: &str, args: &[String], spawned: &Decoded) -> Result<(), Stop> {
        let alloc = Alloc::global();
        let script = Arc::clone(&self.script);
        let refused = |why: String| SpawnRefusal::Fact { program: program.to_owned(), args: args.to_vec(), why };
        let Some(pins) = &script.spawn_setting.pins else {
            return Err(self.refuse(Refusal::Spawn(refused("the command gave no pins for derived facts".to_owned()))));
        };
        let pins = facts::Pins {
            clang_version_text: &pins.clang_version_text,
            macos_product_version: &pins.macos_product_version,
            macos_build_version: &pins.macos_build_version,
        };
        let answer = match facts::answer(fact, &pins, alloc) {
            Ok(answer) => answer,
            Err(why) => return Err(self.refuse(Refusal::Spawn(refused(format!("{why:?}"))))),
        };
        let subject = facts::subject(fact, alloc);
        let digest = reply_digest(answer.code, answer.stdout.as_bytes(), answer.stderr.as_bytes());
        self.record(subject.as_str().to_owned(), Some(digest));
        self.deliver(program, spawned.modes, answer.code, answer.stdout.as_bytes(), answer.stderr.as_bytes())
    }

    /// The spawn sent to the executor as a request record; its written files applied to
    /// `OUT_DIR`, its answer recorded as `spawn:<request key hex>`.
    pub(crate) fn spawn_executor(&mut self, program: &str, args: &[String], spawned: &Decoded) -> Result<(), Stop> {
        let alloc = Alloc::global();
        let script = Arc::clone(&self.script);
        let setting = &script.spawn_setting;
        let no_sandbox = |why: NoSandbox| Refusal::Spawn(SpawnRefusal::NoSandbox { program: program.to_owned(), args: args.to_vec(), why });
        let Some(executor) = script.spawner.as_ref() else {
            return Err(self.refuse(no_sandbox(setting.no_sandbox())));
        };
        let name = program.rsplit('/').next().unwrap_or(program);
        let Some(tool) = setting.tools.iter().find(|tool| tool.name == name) else {
            return Err(self.refuse(no_sandbox(NoSandbox::ToolNotPinned)));
        };
        let personality = if script.host.ends_with("-apple-darwin") { Personality::Xnu } else { Personality::Linux };
        let userland = match (setting.userland, personality) {
            (Some(userland), _) => userland,
            (None, Personality::Xnu) => return Err(self.refuse(no_sandbox(NoSandbox::NoAppleUserland))),
            (None, Personality::Linux) => {
                return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Executor(format!("`{program}`: the command names no guest userland for a Linux tool")))));
            }
        };
        let Tree::Mem { files: tree, .. } = &script.tree else {
            return Err(self.refuse(Refusal::Spawn(SpawnRefusal::TreeOnDisk { program: program.to_owned(), args: args.to_vec() })));
        };
        let out_dir = script.out_dir();
        let text = |value: &str| blaze_own::String::from_str_in(value, alloc);

        let program_tool = Tool { name: text(&tool.name), digest: tool.digest, guest_path: text(&tool.guest_path) };
        let mut tools = blaze_own::Vec::new_in(alloc);
        for other in setting.tools.iter().filter(|other| other.name != tool.name) {
            tools.push(Tool { name: text(&other.name), digest: other.digest, guest_path: text(&other.guest_path) });
        }
        let mut arg_list = blaze_own::Vec::new_in(alloc);
        for arg in args {
            arg_list.push(text(arg));
        }
        let mut env = blaze_own::Vec::new_in(alloc);
        for (key, value) in child_env(&script.env, spawned) {
            env.push((text(&key), text(&value)));
        }
        let cwd = spawned.cwd.clone().unwrap_or_else(|| script.cwd());
        let mut stdin = blaze_own::Vec::new_in(alloc);
        stdin.extend_from_slice(&spawned.stdin);
        let mut reads = blaze_own::Vec::new_in(alloc);
        reads.push(Input::Tree { path: text(WORKSPACE), digest: tree_digest(tree) });
        let mut writes = blaze_own::Vec::new_in(alloc);
        writes.push(text(out_dir));
        let mut present = blaze_own::Vec::new_in(alloc);
        for (rel, bytes) in self.out_files() {
            present.push((text(&format!("{out_dir}/{rel}")), file_digest(bytes)));
        }
        let request = match SpawnRequest::new(
            script.step,
            personality,
            MACHINE_V1,
            userland,
            program_tool,
            tools,
            arg_list,
            env,
            text(&cwd),
            stdin,
            reads,
            writes,
            present,
        ) {
            Ok(request) => request,
            Err(why) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Executor(format!("`{program}`: its spawn request is refused: {why:?}"))))),
        };
        let key = request.key(alloc);
        let answered = executor.answer(&request, &SpawnFiles { tree, sandbox: self, out_dir }, alloc);
        let answer: SpawnAnswer<'_> = match answered {
            Ok(answer) => answer,
            Err(SpawnRefused::NoSandbox { why, .. }) => return Err(self.refuse(no_sandbox(why))),
            Err(other) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Executor(other.to_string())))),
        };
        let code = match answer.status {
            Status::Code(code) => code,
            Status::Signal(signal) => {
                return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Executor(format!(
                    "`{program}` was ended by signal {signal}, which a build script's spawn reply cannot carry"
                )))));
            }
        };
        self.apply_written(program, out_dir, answer.written.as_slice())?;
        self.record(format!("spawn:{}", hex(&key.0)), Some(answer.digest(alloc)));
        self.deliver(program, spawned.modes, code, answer.stdout.as_slice(), answer.stderr.as_slice())
    }

    /// The files an answer wrote or removed, applied to `OUT_DIR` through its limits: a path
    /// outside `OUT_DIR`, or one climbing, is refused by name; a missing folder on the way is
    /// made, counted as a file.
    fn apply_written(&mut self, program: &str, out_dir: &str, written: &[Written<'_>]) -> Result<(), Stop> {
        for one in written {
            let path = one.path.as_str();
            let rel = path.strip_prefix(out_dir).and_then(|rest| rest.strip_prefix('/')).unwrap_or("");
            if rel.is_empty() || rel.split('/').any(|part| matches!(part, "" | "." | "..")) {
                return Err(self.refuse(Refusal::Path(format!("the spawned `{program}` wrote {path}, outside OUT_DIR, the only folder a build script's spawn may write"))));
            }
            match &one.bytes {
                Some(bytes) => {
                    let mut at = 0;
                    while let Some(slash) = rel[at..].find('/') {
                        let folder = &rel[..at + slash];
                        match self.out.get(folder) {
                            Some(None) => {}
                            Some(Some(_)) => {
                                return Err(self.refuse(Refusal::Path(format!("the spawned `{program}` wrote {path}, under a file of OUT_DIR"))));
                            }
                            None => {
                                self.count_file(folder)?;
                                self.out.insert(folder.to_owned(), None);
                            }
                        }
                        at += slash + 1;
                    }
                    let old = match self.out.get(rel) {
                        Some(None) => {
                            return Err(self.refuse(Refusal::Path(format!("the spawned `{program}` wrote {path}, which is a folder of OUT_DIR"))));
                        }
                        Some(Some(old)) => Some(old.len() as u64),
                        None => None,
                    };
                    let new = bytes.len() as u64;
                    self.grow(new.saturating_sub(old.unwrap_or(0)))?;
                    if old.is_none() {
                        self.count_file(rel)?;
                    }
                    self.out.insert(rel.to_owned(), Some(bytes.as_slice().to_vec()));
                    self.out_bytes = self.out_bytes - old.unwrap_or(0) + new;
                }
                None => match self.out.get(rel) {
                    Some(Some(old)) => {
                        let len = old.len() as u64;
                        self.out.remove(rel);
                        self.out_bytes -= len;
                    }
                    Some(None) if self.out.keys().any(|key| key.len() > rel.len() && key.starts_with(rel) && key.as_bytes()[rel.len()] == b'/') => {
                        return Err(self.refuse(Refusal::Path(format!("the spawned `{program}` removed {path}, a folder of OUT_DIR that is not empty"))));
                    }
                    Some(None) => {
                        self.out.remove(rel);
                    }
                    None => {
                        return Err(self.refuse(Refusal::Path(format!("the spawned `{program}` removed {path}, which OUT_DIR does not hold"))));
                    }
                },
            }
        }
        Ok(())
    }
}
