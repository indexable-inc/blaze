#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use blaze_codegen::{
    Arch,
    plan::{Mode, OptLevel, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Relocation, Sanitize, Toolchain},
};
use blaze_ids::ContentId;

use super::{Launch, NOT_IN_PROCESS, launch_of};

fn policy(level: OptLevel) -> Policy {
    Policy {
        toolchain: Toolchain {
            id: ContentId([1; 32]),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            arch: Arch::X86_64,
            cpu: "x86-64".to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: Some("23.1".to_owned()),
            stencils: None,
            frame_pointers: true,
            relocation: Relocation::Pic,
        },
        profile: Profile {
            opt_level: level,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::Full,
            panic: Panic::Unwind,
            packages: BTreeMap::new(),
            build_override: Override::default(),
        },
        mode: Mode::Output,
        fold: true,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
    }
}

#[test]
fn a_dev_process_runs_in_process_and_says_what_blaze_lacks() {
    for (profile, level) in [("dev", OptLevel::O0), ("test", OptLevel::O0), ("dev", OptLevel::O1), ("mine", OptLevel::O0)] {
        let why = launch_of(profile, "app", &policy(level)).unwrap_err();
        assert_eq!(why, format!("`app` runs under profile `{profile}`: {NOT_IN_PROCESS}"));
    }
}

#[test]
fn a_release_process_starts_from_its_file() {
    for (profile, level) in [
        ("release", OptLevel::O3),
        ("bench", OptLevel::O3),
        ("release", OptLevel::O0),
        ("dev", OptLevel::O2),
        ("small", OptLevel::Z),
    ] {
        assert_eq!(launch_of(profile, "app", &policy(level)), Ok(Launch::File), "{profile}");
    }
}

#[test]
fn the_build_written_whole_does_not_decide_the_start() {
    // A run's build links a file today; the start is the profile's, as a live build's would be.
    let mut live = policy(OptLevel::O0);
    live.mode = Mode::Live;
    assert_eq!(launch_of("dev", "app", &live), launch_of("dev", "app", &policy(OptLevel::O0)));
}
