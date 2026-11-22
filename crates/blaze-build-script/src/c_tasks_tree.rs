//! A build script's process-tree requests run as ONE task (c.md D174, design A): the part of
//! it that does no IO. tikv-jemalloc-sys's script spawns `sh <OUT_DIR>/build/configure ...`,
//! `make -j 1` and `make -j 1 install_lib_static install_include`, each in `<OUT_DIR>/build`
//! with `CC`, `CFLAGS`, `LDFLAGS` and `CPPFLAGS` set; `check` answers each "exit 0" and keeps
//! it (c_tasks.rs [`crate::c_tasks::Request::Tree`]) with the folder it was spawned in and its
//! environment (outputs.rs [`crate::outputs::CTree`]). `run` and `test` need what the tree
//! makes (`build/lib/libjemalloc_pic.a`, the headers it installs), so the tree runs once, its
//! requests in spawn order against one `OUT_DIR` laid from the state they saw, and its files
//! are a record found by key next time (rule 77: a second run executes nothing).
//!
//! - [`tree_job`]: the requests read into steps (argv, folder, environment), refused by name
//!   when a request has no recorded folder ([`TreeError::NoContext`]: a record made before the
//!   sandbox kept them), when two saw different `OUT_DIR` states ([`TreeError::StatesDiffer`]:
//!   the script wrote between them, and one laid folder cannot be both), or when one ran
//!   outside the guest `OUT_DIR` ([`TreeError::CwdOutside`]: the package is read-only);
//! - [`tree_key`]: what the tree's files are a function of (below);
//! - [`run_tree`]: the reference driver over the [`TreeRunner`] seam (rule 78: the sandboxed
//!   host-tool runner of blaze-proc-macro native_c_tasks.rs now; a deterministic sandbox
//!   later) and the store: found by key, or run, checked and recorded.
//!
//! THE KEY. The steps as recorded (each argv joined by NUL, its guest folder, its environment
//! with guest spellings: the host folders never enter it), the `OUT_DIR` state they saw by
//! content ([`state_digest`]), the C toolchain (the shelf's digest, the SDK's identity, the
//! target), each host tool the runner grants by its SHA-256 and its row's `runs_out_dir`
//! (`crate::key::HostToolKey`, D71), the OS's build (the host tools' libraries and `/bin/sh`'s
//! shell are the OS's: its build names them all; D112 interim), each laid source root by its
//! digest (the package), the runner's form (what it adds to the environment and how it lays
//! the state) and [`FIXED_ENV`]. The demand is not in it: the tree makes what it makes.
//!
//! THE RECORD at the key ([`TREE_MAGIC`]): the files the tree made or changed under `OUT_DIR`,
//! each its name, length, mode and SHA-256, sorted by name; their bytes are in the home's file
//! area. Nothing here reads a clock, a host path or the environment (rule 75).

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::c_tasks::{CStore, FORM, GuestRoot, GuestRoots, LayError, Refused, Toolchain};
use crate::key::HostToolKey;
use crate::outputs::{CTreeRef, FileMode, OutFileRef, OutputsRef, Snap, StoredFile};

/// The variables every tree runs with, whatever the script gave, sorted: one language, one
/// clock (`SOURCE_DATE_EPOCH`, which the run's fixed clock also reads), one zone, and archives
/// with no dates (Apple's `ar` and `ranlib` read `ZERO_AR_DATE`; the shelf's `llvm-ar` writes
/// none by default).
pub const FIXED_ENV: [(&str, &str); 4] = [("LC_ALL", "C"), ("SOURCE_DATE_EPOCH", "0"), ("TZ", "UTC"), ("ZERO_AR_DATE", "1")];

/// How many of the last lines of a failed step's standard error an error keeps.
pub const TREE_LINES: usize = 20;

/// The most files one tree's record may hold (rule 18): `make install` of a C library leaves a
/// few hundred objects and headers.
pub const MOST_TREE_FILES: usize = 1 << 14;

/// A tree record's first bytes.
pub const TREE_MAGIC: &[u8; 5] = b"BCTR1";

/// One recorded tree request: its index, its argv joined by NUL, and its folder and
/// environment.
#[derive(Clone, Copy, Debug)]
pub struct TreeStep<'b> {
    pub request: u32,
    pub text: &'b str,
    pub tree: CTreeRef<'b>,
}

impl<'b> TreeStep<'b> {
    /// Its argv, program first.
    pub fn argv(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.text.split('\0')
    }
}

/// A crate's tree requests, in spawn order, and the one `OUT_DIR` state they saw.
#[derive(Debug)]
pub struct TreeJob<'a, 'b> {
    pub steps: AVec<'a, TreeStep<'b>>,
    pub state: u32,
}

/// What the tree's files are a function of beyond its record ([`tree_key`]).
#[derive(Clone, Copy, Debug)]
pub struct TreeInputs<'i> {
    pub toolchain: &'i Toolchain<'i>,
    /// Each granted program by its SHA-256, in the table's order.
    pub tools: &'i [HostToolKey<'i>],
    /// The OS's build (`25B78`): the host tools' libraries and shells.
    pub os_build: &'i str,
    /// Each laid source root: its guest spelling and its digest.
    pub sources: &'i [(&'i str, [u8; 32])],
    /// The runner's form: a change to what it adds or how it lays takes another.
    pub runner: &'i str,
}

/// How a step that did not succeed ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeExit {
    Code(i32),
    Signal(i32),
    /// The tree's deadline passed; its process group was killed.
    Deadline,
    /// A stream or a budget passed its cap; its process group was killed.
    Over,
}

impl core::fmt::Display for TreeExit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TreeExit::Code(code) => write!(f, "exit {code}"),
            TreeExit::Signal(signal) => write!(f, "signal {signal}"),
            TreeExit::Deadline => write!(f, "stopped at the tree's deadline, its process group killed (rule 18)"),
            TreeExit::Over => write!(f, "stopped past its output or budget cap, its process group killed (rule 18)"),
        }
    }
}

/// What a run of the tree came to.
#[derive(Debug)]
pub enum TreeRan<'a> {
    /// Every step exited 0: the files it made or changed, stored in the file area, and how many
    /// it made that are not kept (a file holding the run's host folder: a log, a depfile).
    Made { files: AVec<'a, StoredFile<'a>>, left_out: u32 },
    /// Step `request` did not succeed: how, and its last [`TREE_LINES`] lines of standard error
    /// with the host folders shown as their guest spellings.
    Failed { request: u32, exit: TreeExit, last_lines: AString<'a> },
    /// Every step exited 0, and the tree made `file` (relative to `OUT_DIR`) holding the host
    /// folder `host`, and `file` is none [`left_out_holding_a_host_folder`] lets go: a record of
    /// it would hold this machine (rule 25), and leaving it out would leave a later step to find
    /// it missing with no cause given (rule 3).
    HostPath { file: AString<'a>, host: AString<'a> },
}

/// Whether a file the tree made that holds a host folder is let go (counted in
/// [`TreeRan::Made`]'s `left_out`, never stored): a log (`*.log`: configure's `config.log`)
/// or a depfile (`*.d`: the compiler's `-MD` output, make's own input), which only the build
/// system that wrote it reads, and only in the folder it was written in. Any other is
/// [`TreeRan::HostPath`]. `name` is relative to `OUT_DIR`.
pub fn left_out_holding_a_host_folder(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.ends_with(".log") || base.ends_with(".d")
}

/// The tree strategy (rule 78): runs `job`'s steps in order against `outputs`' state
/// `job.state` laid as the guest `OUT_DIR`, each step in its recorded folder with its recorded
/// environment, and stores what it made. `key` names the run's folder.
pub trait TreeRunner {
    fn run<'a>(&mut self, job: &TreeJob<'_, '_>, outputs: &OutputsRef<'_>, key: ContentId, alloc: Alloc<'a>) -> Result<TreeRan<'a>, Refused<'a>>;
}

/// The tree's files, found or made.
#[derive(Debug)]
pub struct TreeDone<'a> {
    pub key: ContentId,
    /// Sorted by name.
    pub files: AVec<'a, StoredFile<'a>>,
    /// Whether it ran now.
    pub ran: bool,
    pub left_out: u32,
}

/// Why a crate's tree gave no files. Each names the crate (rule 19).
#[derive(Debug)]
pub enum TreeError<'a, 'b> {
    /// The crate's plan names no tree request.
    NoTrees { package: &'b str },
    /// Request `request` has no recorded folder and environment: its record was made before the
    /// script sandbox kept them (outputs.rs `CTree`); the script must run again.
    NoContext { package: &'b str, request: u32 },
    /// Request `request` saw `OUT_DIR` state `seen`, an earlier one `first`.
    StatesDiffer { package: &'b str, request: u32, first: u32, seen: u32 },
    /// Request `request` was spawned in `cwd`, outside the guest `OUT_DIR`.
    CwdOutside { package: &'b str, request: u32, cwd: &'b str },
    /// The state cannot be read from the record.
    State { package: &'b str, why: LayError<'b> },
    Failed { package: &'b str, request: u32, command: AString<'a>, exit: TreeExit, last_lines: AString<'a> },
    /// The runner could not run it (the sandbox, a tool not granted, the disk).
    Refused { package: &'b str, words: AString<'a> },
    /// The runner gave a file name no record may hold, or one name twice, or too many files.
    BadFile { package: &'b str, name: AString<'a> },
    /// The tree made `file` holding the host folder `host` ([`TreeRan::HostPath`]).
    HostPath { package: &'b str, file: AString<'a>, host: AString<'a> },
    Store { package: &'b str },
    Damaged { package: &'b str, key: ContentId },
}

impl core::fmt::Display for TreeError<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TreeError::NoTrees { package } => write!(f, "the C of {package}: no process-tree request to run"),
            TreeError::NoContext { package, request } => write!(
                f,
                "the C of {package}: process-tree request {request} has no recorded folder and environment (outputs.rs `CTree`): its build script's record must be made again"
            ),
            TreeError::StatesDiffer { package, request, first, seen } => write!(
                f,
                "the C of {package}: process-tree request {request} saw OUT_DIR state {seen}, an earlier one state {first}: the script wrote between them, and one tree runs against one state"
            ),
            TreeError::CwdOutside { package, request, cwd } => {
                write!(f, "the C of {package}: process-tree request {request} was spawned in `{cwd}`, outside its OUT_DIR (the package is read-only)")
            }
            TreeError::State { package, why } => write!(f, "the C of {package}: its process tree's OUT_DIR: {why}"),
            TreeError::Failed { package, request, command, exit, last_lines } => {
                write!(f, "the C of {package}: process-tree request {request} failed ({exit}): {}\n{}", command.as_str(), last_lines.as_str())
            }
            TreeError::Refused { package, words } => write!(f, "the C of {package}: its process tree could not be run: {}", words.as_str()),
            TreeError::BadFile { package, name } => write!(f, "the C of {package}: its process tree left `{}`, which a record cannot hold (a bad name, twice, or past {MOST_TREE_FILES} files)", name.as_str()),
            TreeError::HostPath { package, file, host } => write!(
                f,
                "the C of {package}: its process tree made `$OUT_DIR/{}`, which holds this machine's folder `{}`; no record may hold a machine's path (rule 25: equal inputs, equal bytes), and only a log or a depfile is let go (c_tasks_tree.rs `left_out_holding_a_host_folder`)",
                file.as_str(),
                host.as_str()
            ),
            TreeError::Store { package } => write!(f, "the C of {package}: the store refused its process tree's record"),
            TreeError::Damaged { package, key } => write!(f, "the C of {package}: the process-tree record at {key:?} is not of its form (c_tasks_tree.rs `TREE_MAGIC`)"),
        }
    }
}

/// The crate's tree requests `trees` (c_tasks.rs [`crate::c_tasks::Plan::trees`]) read into
/// one job.
pub fn tree_job<'a, 'b>(package: &'b str, outputs: &OutputsRef<'b>, roots: &GuestRoots<'_>, trees: &[u32], alloc: Alloc<'a>) -> Result<TreeJob<'a, 'b>, TreeError<'a, 'b>> {
    let mut steps: AVec<'a, TreeStep<'b>> = Vec::with_capacity_in(trees.len(), alloc);
    let mut state: Option<u32> = None;
    for &request in trees {
        let missing = TreeError::NoContext { package, request };
        let Some(text) = outputs.c_requests().nth(request as usize) else { return Err(missing) };
        let Some(seen) = outputs.c_request_states().nth(request as usize) else { return Err(missing) };
        let Some(tree) = outputs.c_tree(request) else { return Err(missing) };
        match state {
            None => state = Some(seen),
            Some(first) if first != seen => return Err(TreeError::StatesDiffer { package, request, first, seen }),
            Some(_) => {}
        }
        if roots.root_of(tree.cwd) != Some(GuestRoot::Out) {
            return Err(TreeError::CwdOutside { package, request, cwd: tree.cwd });
        }
        steps.push(TreeStep { request, text, tree });
    }
    let Some(state) = state else { return Err(TreeError::NoTrees { package }) };
    Ok(TreeJob { steps, state })
}

/// The `OUT_DIR` state `state` by content: each entry's name, kind and bytes' digest (a final
/// file's by the record's own file of its name).
pub fn state_digest<'b>(outputs: &OutputsRef<'b>, state: u32) -> Result<[u8; 32], LayError<'b>> {
    let entries = outputs.c_state(state).ok_or(LayError::NoState { state })?;
    let mut hasher = Hasher::new(FORM);
    hasher.part(b"tree state");
    for (name, snap) in entries {
        hasher.part(name.as_bytes());
        match snap {
            Snap::Folder => {
                hasher.update(&[0]);
            }
            Snap::Held(digest) => {
                hasher.update(&[1]).update(&digest);
            }
            Snap::Final => match outputs.file(name) {
                Some(OutFileRef::Inline(bytes)) => {
                    hasher.update(&[1]).update(&crate::outputs::snap_digest(bytes));
                }
                Some(OutFileRef::Stored(_)) | None => return Err(LayError::Unheld { state, name }),
            },
        }
    }
    Ok(hasher.finish().0)
}

/// The tree's key (module doc, THE KEY).
pub fn tree_key(job: &TreeJob<'_, '_>, outputs: &OutputsRef<'_>, inputs: &TreeInputs<'_>, state: [u8; 32]) -> ContentId {
    let t = inputs.toolchain;
    let mut hasher = Hasher::new(FORM);
    hasher
        .part(b"tree")
        .update(&[t.kind as u8])
        .part(&t.compiler)
        .part(t.sdk)
        .part(t.target.as_bytes())
        .part(inputs.os_build.as_bytes())
        .part(inputs.runner.as_bytes())
        .part(outputs.out_dir().as_bytes())
        .part(&state);
    hasher.update(&(inputs.tools.len() as u64).to_le_bytes());
    for tool in inputs.tools {
        hasher.part(tool.name.as_bytes()).part(&tool.digest).update(&[u8::from(tool.runs_out_dir)]);
    }
    hasher.update(&(inputs.sources.len() as u64).to_le_bytes());
    for (guest, digest) in inputs.sources {
        hasher.part(guest.as_bytes()).part(digest);
    }
    hasher.update(&(FIXED_ENV.len() as u64).to_le_bytes());
    for (name, value) in FIXED_ENV {
        hasher.part(name.as_bytes()).part(value.as_bytes());
    }
    hasher.update(&(job.steps.len() as u64).to_le_bytes());
    for step in job.steps.iter() {
        hasher.part(step.text.as_bytes()).part(step.tree.cwd.as_bytes());
        hasher.update(&(step.tree.env().count() as u64).to_le_bytes());
        for (name, value) in step.tree.env() {
            hasher.part(name.as_bytes()).part(value.as_bytes());
        }
    }
    hasher.finish()
}

/// `files` (sorted, checked) as a tree record into `out`.
pub fn encode_tree(files: &[StoredFile<'_>], out: &mut AVec<'_, u8>) {
    out.clear();
    out.extend_from_slice(TREE_MAGIC);
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for file in files {
        out.extend_from_slice(&(file.name.len() as u32).to_le_bytes());
        out.extend_from_slice(file.name.as_bytes());
        out.extend_from_slice(&file.len.to_le_bytes());
        out.extend_from_slice(&file.mode.word().to_le_bytes());
        out.extend_from_slice(&file.sha256);
    }
}

/// A tree record's files, checked as [`check_files`] checks a run's; `None` when the bytes are
/// not one.
pub fn decode_tree<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Option<AVec<'a, StoredFile<'a>>> {
    let mut cur = bytes.strip_prefix(TREE_MAGIC.as_slice())?;
    let n = word(take(&mut cur, 4)?)? as usize;
    if n > MOST_TREE_FILES {
        return None;
    }
    let mut files: AVec<'a, StoredFile<'a>> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        let len = word(take(&mut cur, 4)?)? as usize;
        let name = core::str::from_utf8(take(&mut cur, len)?).ok()?;
        let size = u64::from_le_bytes(<[u8; 8]>::try_from(take(&mut cur, 8)?).ok()?);
        let mode = FileMode::of_word(word(take(&mut cur, 4)?)?)?;
        let sha256 = <[u8; 32]>::try_from(take(&mut cur, 32)?).ok()?;
        files.push(StoredFile { name: String::from_str_in(name, alloc), len: size, mode, sha256 });
    }
    if !cur.is_empty() {
        return None;
    }
    check_files(&mut files).ok()?;
    Some(files)
}

/// The next `n` bytes of `cur`, which moves past them.
fn take<'r>(cur: &mut &'r [u8], n: usize) -> Option<&'r [u8]> {
    let (head, rest) = (cur.get(..n)?, cur.get(n..)?);
    *cur = rest;
    Some(head)
}

fn word(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(<[u8; 4]>::try_from(bytes).ok()?))
}

/// `files` sorted by name; the first name a record cannot hold (empty, absolute, a `.` or
/// `..` or empty part), held twice, or past [`MOST_TREE_FILES`], refused.
pub fn check_files<'f>(files: &'f mut AVec<'_, StoredFile<'_>>) -> Result<(), &'f str> {
    files.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    let bad = |name: &str| name.is_empty() || name.starts_with('/') || name.split('/').any(|part| matches!(part, "" | "." | ".."));
    let mut fault: Option<usize> = None;
    for at in 0..files.len() {
        let name = files[at].name.as_str();
        if bad(name) || at >= MOST_TREE_FILES || (at > 0 && files[at - 1].name.as_str() == name) {
            fault = Some(at);
            break;
        }
    }
    match fault {
        Some(at) => Err(files[at].name.as_str()),
        None => Ok(()),
    }
}

/// The last `most` lines of `text`, each line as text (a line that is not UTF-8 named so).
pub fn last_lines<'a>(text: &[u8], most: usize, alloc: Alloc<'a>) -> AString<'a> {
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    let lines = text.split(|b| *b == b'\n').count();
    let mut out = String::new_in(alloc);
    for line in text.split(|b| *b == b'\n').skip(lines.saturating_sub(most)) {
        out.push_str(core::str::from_utf8(line).unwrap_or("(a line that is not UTF-8)"));
        out.push('\n');
    }
    out
}

/// A step's text as a command line (its argv, `\0` a space): guest paths, as the script ran it.
fn command<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for (at, word) in text.split('\0').enumerate() {
        if at > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// The reference driver: the job, its key, one get; on a miss the runner runs it, and its
/// files (sorted, checked) are recorded at the key.
#[expect(clippy::too_many_arguments, reason = "each is one seam or input, named at the call")]
pub fn run_tree<'a, 'b>(
    package: &'b str,
    outputs: &OutputsRef<'b>,
    roots: &GuestRoots<'_>,
    trees: &[u32],
    inputs: &TreeInputs<'_>,
    runner: &mut dyn TreeRunner,
    store: &mut dyn CStore,
    alloc: Alloc<'a>,
) -> Result<TreeDone<'a>, TreeError<'a, 'b>> {
    let job = tree_job(package, outputs, roots, trees, alloc)?;
    let state = state_digest(outputs, job.state).map_err(|why| TreeError::State { package, why })?;
    let key = tree_key(&job, outputs, inputs, state);
    let mut held: AVec<'_, u8> = Vec::new_in(alloc);
    if store.get(key, &mut held).map_err(|_| TreeError::Store { package })? {
        let files = decode_tree(&held, alloc).ok_or(TreeError::Damaged { package, key })?;
        return Ok(TreeDone { key, files, ran: false, left_out: 0 });
    }
    let (mut files, left_out) = match runner.run(&job, outputs, key, alloc) {
        Ok(TreeRan::Made { files, left_out }) => (files, left_out),
        Ok(TreeRan::Failed { request, exit, last_lines }) => {
            let text = job.steps.iter().find(|step| step.request == request).map_or("", |step| step.text);
            return Err(TreeError::Failed { package, request, command: command(text, alloc), exit, last_lines });
        }
        Ok(TreeRan::HostPath { file, host }) => return Err(TreeError::HostPath { package, file, host }),
        Err(refused) => return Err(TreeError::Refused { package, words: refused.words }),
    };
    if let Err(name) = check_files(&mut files) {
        return Err(TreeError::BadFile { package, name: String::from_str_in(name, alloc) });
    }
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    encode_tree(&files, &mut bytes);
    store.put(key, &bytes).map_err(|_| TreeError::Store { package })?;
    Ok(TreeDone { key, files, ran: true, left_out })
}
