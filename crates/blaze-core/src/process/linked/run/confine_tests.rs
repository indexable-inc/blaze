//! The child's confine step (D177) on the host's kernel (kind `process`: they start processes).
//! A `/bin/sh` child is started by the one linked start with a confinement made here from
//! blaze-sys's wrappers: Landlock handling file reads only, granted beneath the system folders
//! and one folder of the test's own; the seccomp filter blaze-sys builds; one limit. The kernel
//! must let it read inside and refuse the file beside it, and a confinement the kernel refuses
//! must come back as the typed step, its program never run.

use std::{ffi::OsString, path::PathBuf, time::Duration};

use blaze_sys::linux::{
    AccessFs, CWD, OpenFlags, PathBeneath, RulesetAttr, SockFilter,
    confine::{ChildConfine, Filter, MOST_FILTER, Resource, Rlimit, Step, build},
    landlock_add_rule_path, landlock_create_ruleset, openat,
};

use super::*;
use crate::{linked::Waited, wait::Deadline};

/// Folders `sh` and `cat` are read from: the loader, the libraries, the binaries.
const SYSTEM: [&std::ffi::CStr; 5] = [c"/bin", c"/usr", c"/lib", c"/etc", c"/lib64"];

fn sh(script: &str, dir: &std::path::Path) -> Command {
    Command {
        program: PathBuf::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from(script)],
        env: vec![(OsString::from("PATH"), OsString::from("/bin:/usr/bin"))],
        dir: dir.to_path_buf(),
        stdin: Vec::new(),
        sandbox: Sandbox::Off,
        memory: None,
    }
}

fn exit_of(command: &Command, confine: &ChildConfine<'_>) -> Result<Exit, LinkError> {
    let null = Streams { stdin: Input::Null, stdout: Output::Null, stderr: Output::Null };
    let link = RunLink { inherit: &[], own_group: true, streams: null, arg0: None, parent_death: ParentDeath::Kernel, confine: Some(confine) };
    let started = start_run(command, &link)?;
    match started.child.wait_until(Deadline::at(Duration::from_secs(20)))? {
        Waited::Ended(exit) => Ok(exit),
        Waited::Running(_) => panic!("still running at the test's bound"),
    }
}

/// Prints "confine step: inside read, beside refused, a refused step typed".
#[test]
fn the_child_enters_its_confinement_before_its_program() {
    let base = std::env::temp_dir().join(format!("blaze-d177-core-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("inside")).unwrap();
    std::fs::write(base.join("inside/in.txt"), b"in").unwrap();
    std::fs::write(base.join("beside.txt"), b"beside").unwrap();
    let base = std::fs::canonicalize(&base).unwrap();

    let attr = RulesetAttr { handled_access_fs: AccessFs::READ_FILE.bits(), handled_access_net: 0, scoped: 0 };
    let ruleset = landlock_create_ruleset(&attr).expect("Landlock (ABI 1 or later)");
    let inside = std::ffi::CString::new(base.join("inside").into_os_string().into_encoded_bytes()).unwrap();
    let mut held = Vec::new();
    for path in SYSTEM.iter().copied().chain([inside.as_c_str()]) {
        // A folder the host lacks (`/lib64` on aarch64) is no rule.
        let Ok(fd) = openat(CWD, blaze_sys::Path::new(path), OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC), 0) else { continue };
        let rule = PathBeneath { allowed_access: AccessFs::READ_FILE.bits(), parent_fd: fd.fd().raw() };
        landlock_add_rule_path(ruleset.fd(), &rule).expect("a rule");
        held.push(fd);
    }
    let mut filter = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
    let filter = build(Filter { processes: true }, &mut filter);
    let limits = [(Resource::Core, Rlimit { soft: 0, hard: 0 })];
    let confine = ChildConfine { ruleset: ruleset.fd(), filter, limits: &limits };

    // `cat` of the inside file succeeds (3 otherwise); of the file beside it fails (4 otherwise).
    let script = "cat inside/in.txt >/dev/null || exit 3; cat beside.txt >/dev/null 2>&1 && exit 4; exit 0";
    assert_eq!(exit_of(&sh(script, &base), &confine).expect("the confined start"), Exit::Exited(0));

    // Unconfined, the same script reads the file beside it: the confinement is the cause.
    let unconfined = RunLink {
        inherit: &[],
        own_group: true,
        streams: Streams { stdin: Input::Null, stdout: Output::Null, stderr: Output::Null },
        arg0: None,
        parent_death: ParentDeath::Kernel,
        confine: None,
    };
    let started = start_run(&sh(script, &base), &unconfined).expect("the plain start");
    match started.child.wait_until(Deadline::at(Duration::from_secs(20))).expect("a wait") {
        Waited::Ended(exit) => assert_eq!(exit, Exit::Exited(4)),
        Waited::Running(_) => panic!("still running at the test's bound"),
    }

    // A ruleset that is no descriptor: the kernel refuses `landlock_restrict_self`, the start
    // names that step, and the program never ran (it would have made `ran`).
    let ran = base.join("ran");
    let broken = ChildConfine { ruleset: blaze_sys::Fd::new(-1), filter, limits: &limits };
    let error = exit_of(&sh("touch ran", &base), &broken).expect_err("a refused confinement");
    assert_eq!(error.op, LinkOp::Confine(Step::Landlock));
    assert!(!ran.exists(), "the program ran after its confinement was refused");

    drop(held);
    _ = std::fs::remove_dir_all(&base);
    println!("confine step: inside read, beside refused, a refused step typed");
}
