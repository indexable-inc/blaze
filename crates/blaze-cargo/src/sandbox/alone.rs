//! A test's sandbox, made in a process of its own that is one thread. The sandbox forks, and
//! libtest runs every test on a thread of its own beside its main thread, `--test-threads=1`
//! too: a fork copies the forking thread alone, so a lock another thread held then (the
//! allocator's, stdio's, the environment's) stays held in the child for good, which deadlocks
//! or sees what that thread left half done. `blaze sandbox` is one thread, but a test binary is
//! no blaze: [`alone`] runs the test binary again, and [`take_over`] has it do what was asked
//! as it loads, before `main` and so before libtest starts any thread, and end.

#[cfg(target_os = "linux")]
use std::ffi::OsString;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_usercode::confine;
use serde::{Deserialize, Serialize};

use super::{COULD_NOT, Probe};

/// What [`alone`] gives the test binary it runs again: what to do, as JSON ([`Asked`]).
const VAR: &str = "BLAZE_SANDBOX_TEST_ALONE";

/// The argument it gives it too, which libtest refuses: a run [`take_over`] did not take over
/// fails, instead of running the tests.
const FLAG: &str = "--blaze-sandbox-test-alone";

#[derive(Serialize, Deserialize)]
struct Asked {
    job: Job,
    /// Where the answer goes: `Result<i32, String>`, as JSON.
    answer: PathBuf,
}

#[derive(Serialize, Deserialize)]
enum Job {
    Probe(Probe),
    Launch(confine::Spec),
    /// [`super::linux::run`], or with why namespaces can't be made [`super::linux::run_as`].
    #[cfg(target_os = "linux")]
    Run { spec: super::Spec, program: Option<Vec<OsString>>, unshared: Option<String> },
}

impl Job {
    fn run(self) -> Result<i32, String> {
        match self {
            Job::Probe(kind) => super::probe(kind).map(|()| 0),
            Job::Launch(spec) => Ok(super::v2::launch(&spec)),
            #[cfg(target_os = "linux")]
            Job::Run { spec, program, unshared } => {
                let (program, also) = (program.as_deref(), super::Also::default());
                match unshared {
                    None => super::linux::run(&spec, program, also),
                    Some(why) => super::linux::run_as(&spec, program, Some(why), also),
                }
            }
        }
    }
}

/// [`super::probe`], in a process of its own.
pub(super) fn probe(kind: Probe) -> Result<(), String> {
    alone(Job::Probe(kind)).map(|_| ())
}

/// [`super::v2::launch`], in a process of its own.
pub(super) fn launch(spec: &confine::Spec) -> i32 {
    // clone: once per sandbox a test makes, the job it is sent as.
    match alone(Job::Launch(spec.clone())) {
        Ok(code) => code,
        Err(why) => panic!("`launch` answers a status, not an error: {why}"),
    }
}

/// [`super::linux::run`] with nothing [`super::Also`], in a process of its own.
#[cfg(target_os = "linux")]
pub(super) fn run(spec: &super::Spec, program: Option<&[OsString]>) -> Result<i32, String> {
    let program = program.map(<[OsString]>::to_vec);
    // clone: once per sandbox a test makes, the job it is sent as.
    alone(Job::Run { spec: spec.clone(), program, unshared: None })
}

/// [`super::linux::run_as`] with nothing [`super::Also`], in a process of its own.
#[cfg(target_os = "linux")]
pub(super) fn run_as(
    spec: &super::Spec,
    program: Option<&[OsString]>,
    unshared: String,
) -> Result<i32, String> {
    let program = program.map(<[OsString]>::to_vec);
    // clone: once per sandbox a test makes, the job it is sent as.
    alone(Job::Run { spec: spec.clone(), program, unshared: Some(unshared) })
}

/// [`run`], and also what it and the sandbox wrote, which is not written to this test's stderr.
#[cfg(target_os = "linux")]
pub(super) fn run_said(spec: &super::Spec, program: Option<&[OsString]>) -> (Result<i32, String>, String) {
    let program = program.map(<[OsString]>::to_vec);
    // clone: once per sandbox a test makes, the job it is sent as.
    said(Job::Run { spec: spec.clone(), program, unshared: None })
}

/// Runs this test binary again to do `job`, and what it answered. What it and the sandbox
/// wrote is written to this test's stderr.
fn alone(job: Job) -> Result<i32, String> {
    let (answered, said) = said(job);
    eprint!("{said}");
    answered
}

/// [`alone`], and what it and the sandbox wrote.
fn said(job: Job) -> (Result<i32, String>, String) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // Once per sandbox a test makes: where its answer goes, and the job, for the process that
    // makes it.
    // alloc: as above.
    let answer = std::env::temp_dir().join(format!(
        "blaze-sandbox-alone-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // clone: as above.
    let asked = serde_json::to_string(&Asked { job, answer: answer.clone() }).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(FLAG)
        .env(VAR, asked)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    // alloc: once per sandbox a test makes, what it wrote, read as text.
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let answered = std::fs::read(&answer);
    _ = std::fs::remove_file(&answer);
    match answered.ok().and_then(|it| serde_json::from_slice(&it).ok()) {
        Some(answered) => (answered, said),
        None => panic!("the test binary run again answered nothing ({}): {said}", out.status),
    }
}

/// Does what [`alone`] asked of this run of the test binary, when it was, and ends.
extern "C" fn take_over() {
    let Some(asked) = std::env::var_os(VAR) else { return };
    let Some(Ok(asked)) = asked.to_str().map(serde_json::from_str::<Asked>) else {
        eprintln!("blaze: `{VAR}` is no job");
        std::process::exit(COULD_NOT);
    };
    let answered = serde_json::to_vec(&asked.job.run()).unwrap();
    std::process::exit(match std::fs::write(&asked.answer, answered) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("blaze: {}: {error}", asked.answer.display());
            COULD_NOT
        }
    });
}

// SAFETY: the loader calls each entry of this section as a function as the binary loads,
// before `main`, when the process is one thread (nothing in a test binary starts one before
// `main`). The entry is `take_over`, an `extern "C" fn()`: the loader passes it `argc`, `argv`
// and `envp`, which the C calling convention lets it ignore. Anything but a function pointer
// there would be called as code.
#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
static TAKE_OVER: extern "C" fn() = take_over;
