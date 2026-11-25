//! D69 (Linux): run one command confined by bstd's hermetic run at `Level::Inputs` (D177:
//! Landlock + seccomp + `no_new_privs` on Linux, Seatbelt on macOS), as a subprocess wrapper so
//! a sans-IO builder can confine a build without linking the world itself.
//!
//! `confined_exec <spec.json>`: the spec names the program, its args and exact environment, its
//! working folder, the folders it may read (tree roots), the one writable folder (the output
//! root: the build's `ws`/`target`/`cargo-home`/`tmp` live under it), the programs it may start
//! (`helpers`: single files; `helper_folders`: subtrees the build's own outputs run from), and
//! the budget. The child's stdout and stderr are mirrored to ours, its exit status is ours; a
//! refusal of the spec or the confinement is printed and is exit 125, never a silent unsandboxed
//! run.
//!
//! The tree roots are named by the spec's own digest (the runtime's trick, native_script.rs
//! `RootKind::Tree { tree: Digest(script.key.0) }`): a local build's read set is keyed by the
//! run, not re-hashed; the built module's real inputs are keyed by the builder's own identity
//! (the pins, the lock closure).

use std::{path::Path, process::ExitCode, time::Duration};

use blaze_core::wait::Deadline;
use blaze_own::{AVec, Alloc};
use blaze_roots::{Digest, Mount, RelPath, Root, RootId, RootKind, Roots};
use bstd::{
    args::ArgList,
    env::Vars,
    io::{Io, Limits, NativeWorld},
    mem::TASK_FRAME,
    procs::{
        Exit,
        hermetic::{
            Budget, Capture, ClockSpec, Fault, FilesError, FilesOp, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec,
            HermeticStreams, Level, Program, ReportSpec, RootFiles,
        },
    },
};

const SCRIPT: &str = "confined_exec";

/// The spec's fields, all required unless said.
struct Spec {
    program: String,
    args: std::vec::Vec<String>,
    cwd: String,
    env: std::vec::Vec<(String, String)>,
    reads: std::vec::Vec<String>,
    write: String,
    helpers: std::vec::Vec<String>,
    helper_folders: std::vec::Vec<String>,
    cpu_secs: u64,
    memory_bytes: u64,
    deadline_secs: u64,
}

fn fail(why: &str) -> ! {
    eprintln!("{SCRIPT}: {why}");
    std::process::exit(125);
}

fn string_field<'v>(map: &'v serde_json::Value, key: &str) -> &'v str {
    map.get(key).and_then(serde_json::Value::as_str).unwrap_or_else(|| fail(&format!("the spec has no string `{key}`")))
}

fn string_list(map: &serde_json::Value, key: &str) -> std::vec::Vec<String> {
    map.get(key)
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| fail(&format!("the spec has no list `{key}`")))
        .iter()
        .map(|v| v.as_str().unwrap_or_else(|| fail(&format!("`{key}` holds a non-string"))).to_owned())
        .collect()
}

fn parse(text: &str) -> Spec {
    let value: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|e| fail(&format!("the spec is not JSON: {e}")));
    let env = string_list(&value, "env")
        .iter()
        .map(|pair| {
            let (k, v) = pair.split_once('\u{1f}').unwrap_or_else(|| fail("an env pair is not `K\u{1f}V`"));
            (k.to_owned(), v.to_owned())
        })
        .collect();
    Spec {
        program: string_field(&value, "program").to_owned(),
        args: string_list(&value, "args"),
        cwd: string_field(&value, "cwd").to_owned(),
        env,
        reads: string_list(&value, "reads"),
        write: string_field(&value, "write").to_owned(),
        helpers: string_list(&value, "helpers"),
        helper_folders: string_list(&value, "helper_folders"),
        cpu_secs: value.get("cpu_secs").and_then(serde_json::Value::as_u64).unwrap_or_else(|| fail("the spec has no `cpu_secs`")),
        memory_bytes: value.get("memory_bytes").and_then(serde_json::Value::as_u64).unwrap_or_else(|| fail("the spec has no `memory_bytes`")),
        deadline_secs: value.get("deadline_secs").and_then(serde_json::Value::as_u64).unwrap_or_else(|| fail("the spec has no `deadline_secs`")),
    }
}

/// No package roots (tree roots only), no report program: the trait's methods are never asked
/// for a file; the output root is not collected (the build's outputs stay in its folders).
struct NoFiles;

impl RootFiles for NoFiles {
    fn package_files(&self, root: RootId, _: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        let _ = root;
        Ok(())
    }
    fn seen(&self, root: RootId, _: RelPath<'_>) -> Result<blaze_roots::Seen, Fault> {
        Err(Fault::Files(FilesError { op: FilesOp::Seen, root, code: 0 }))
    }
    fn read_output(&self, _: RelPath<'_>, _: u32, _: &mut AVec<'_, u8>) -> Result<(), Fault> {
        Err(Fault::Files(FilesError { op: FilesOp::ReadOutput, root: RootId(0), code: 0 }))
    }
    fn list_output(&self, _: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        Ok(())
    }
    fn clear_output(&self) -> Result<(), Fault> {
        Ok(())
    }
}

fn main() -> ExitCode {
    let args: std::vec::Vec<String> = std::env::args().collect();
    let [_, spec_path] = args.as_slice() else {
        eprintln!("usage: {SCRIPT} <spec.json>");
        return ExitCode::from(2);
    };
    let text = std::fs::read_to_string(spec_path).unwrap_or_else(|e| fail(&format!("{spec_path}: {e}")));
    let spec = parse(&text);
    let frame = blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Usercode).unwrap_or_else(|e| fail(&format!("no frame: {e:?}")));
    let erased = blaze_own::Erased::new(&frame);
    let alloc: Alloc<'_> = erased.handle();

    let world = Io::native(NativeWorld {
        root: Path::new(&spec.write),
        threads: 1,
        env: Vars::given(Vec::new()),
        args: ArgList::given(Vec::new()),
        host_name: std::sync::Arc::from(""),
        seed: Some(0),
        above: 0,
        limits: Limits::USUAL,
    })
    .unwrap_or_else(|e| fail(&format!("{}: {e}", spec.write)));
    let spawner = world.need_host().and_then(|host| host.spawner()).unwrap_or_else(|m| fail(&format!("no spawner: {}", m.capability)));
    let clock = world.need_clock().unwrap_or_else(|m| fail(&format!("no clock: {}", m.capability)));
    let cx = world.cx(Duration::from_secs(spec.deadline_secs + 60), TASK_FRAME).unwrap_or_else(|_| fail("the world stopped"));
        let deadline = Deadline::after(clock.now().since_start(), Duration::from_secs(spec.deadline_secs));
    let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock });

    // The spec's own digest names every read tree (see the module docs).
    let spec_digest = blaze_sha256::digest(text.as_bytes());
    let tree = RootKind::Tree { tree: Digest(spec_digest) };
    let mut roots: AVec<'_, Root<'_>> = AVec::new_in(alloc);
    let mut mounts: AVec<'_, Mount<'_>> = AVec::new_in(alloc);
    for read in spec.reads.iter() {
        roots.push(Root { name: "a folder the build reads", kind: tree });
        mounts.push(Mount { spelling: read.as_bytes(), root: RootId((roots.len() - 1) as u8) });
    }
    roots.push(Root { name: "the build's writable folder", kind: RootKind::Output });
    mounts.push(Mount { spelling: spec.write.as_bytes(), root: RootId((roots.len() - 1) as u8) });
    let roots = Roots::new(&roots).unwrap_or_else(|e| fail(&format!("roots refused: {e:?}")));

    let program_digest = {
        let bytes = std::fs::read(&spec.program).unwrap_or_else(|e| fail(&format!("{}: {e}", spec.program)));
        blaze_sha256::digest(&bytes)
    };
    let mut env: AVec<'_, (&str, &str)> = AVec::new_in(alloc);
    for (k, v) in spec.env.iter() {
        env.push((k.as_str(), v.as_str()));
    }
    env.sort_by(|a, b| a.0.cmp(b.0));
    let mut helpers: AVec<'_, &str> = AVec::new_in(alloc);
    for helper in spec.helpers.iter() {
        if Path::new(helper).exists() {
            helpers.push(helper.as_str());
        }
    }
    let mut arg_refs: AVec<'_, &str> = AVec::new_in(alloc);
    for arg in spec.args.iter() {
        arg_refs.push(arg.as_str());
    }
    let epoch = spec.env.iter().find(|(k, _)| k == "SOURCE_DATE_EPOCH").and_then(|(_, v)| v.parse::<u64>().ok());
    let clock_spec = match epoch {
        Some(unix_secs) => ClockSpec::Fixed { unix_secs },
        None => ClockSpec::NeverAsks,
    };
    let mut helper_folders: AVec<'_, &str> = AVec::new_in(alloc);
    for folder in spec.helper_folders.iter() {
        helper_folders.push(folder.as_str());
    }
    let hspec = HermeticSpec::new(HermeticOpts {
        program: Program::Stored { name: spec.program.rsplit('/').next().unwrap_or(&spec.program), digest: Digest(program_digest), path: &spec.program },
        args: &arg_refs,
        env: &env,
        roots,
        mounts: &mounts,
        cwd: &spec.cwd,
        helpers: &helpers,
        helper_folders: &helper_folders,
        channels: &[],
        clock: clock_spec,
        seed: 0,
        budget: Budget { cpu: Duration::from_secs(spec.cpu_secs), memory: spec.memory_bytes, steps: None },
        deadline,
        streams: HermeticStreams { stdout: Capture::Keep { most: 64 << 20 }, stderr: Capture::Keep { most: 64 << 20 } },
        level: Level::Inputs,
        report: ReportSpec::WholeRoots,
    })
    .unwrap_or_else(|e| fail(&format!("the spec is refused: {:?}", e.fault)));
    let ran = hermetic.run(&cx, &hspec, &NoFiles, alloc).unwrap_or_else(|e| fail(&format!("the confined run failed: {e}")));
    use std::io::Write as _;
    let _ = std::io::stdout().write_all(&ran.stdout);
    let _ = std::io::stderr().write_all(&ran.stderr);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    match ran.exit {
        Exit::Exited(code) => ExitCode::from(u8::try_from(code).unwrap_or(125)),
        Exit::Signaled(sig) => {
            eprintln!("{SCRIPT}: the child ended by signal {sig}");
            ExitCode::from(125)
        }
    }
}
