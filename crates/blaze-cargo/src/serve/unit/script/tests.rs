use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use blaze_build_script::unit as bs;
use blaze_core::{Completion, Done, Effect, FileName, IoError, Stamp, Tag};
use blaze_usercode::run::OutFile;

use super::{BYTES, CUT, LOOK, LOOK_EVERY, QUIET_MOST, Quiet, Stuck, TICK, TIMED, Writing, file_name, out_files, verifies_in};

const RUN: Tag = Tag(5 << 32);
const BEGAN_NS: u64 = 1_761_782_400_123_456_789;

fn unit() -> bs::Unit {
    let mut unit = bs::Unit::default();
    unit.package.name = "z-sys".into();
    unit.package.version = "0.1.0".into();
    unit.out_dir = PathBuf::from("/w/target/debug/build/z-sys-0123456789abcdef/out");
    unit
}

fn name(path: &str) -> FileName {
    FileName::new(path).unwrap()
}

fn began() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_nanos(BEGAN_NS)
}

fn done(own: u64, done: Done) -> Completion {
    Completion { tag: Tag(RUN.0 + own), done }
}

const DIR: &str = "w/target/debug/build/z-sys-0123456789abcdef";

#[test]
fn blaze_elide_is_verify_or_unset() {
    let env = |value: &str| vec![("PATH".to_owned(), "/usr/bin".to_owned()), ("BLAZE_ELIDE".to_owned(), value.to_owned())];
    assert!(!verifies_in(&[("PATH".to_owned(), "/usr/bin".to_owned())]).unwrap());
    assert!(verifies_in(&env("verify")).unwrap());
    for refused in ["1", "", "Verify", "off"] {
        let error = verifies_in(&env(refused)).unwrap_err();
        assert!(error.what.starts_with(&format!("BLAZE_ELIDE={refused} is not a value blaze takes")), "{}", error.what);
    }
}

#[test]
fn cargos_files_are_cut_then_written_and_output_dated_when_the_run_began() {
    let unit = unit();
    let mut effects = Vec::new();
    let mut writing = Writing::begin(&unit, b"cargo:rustc-cfg=has_z\n".to_vec(), Vec::new(), began(), RUN, &mut effects).unwrap();
    let output = name(&format!("{DIR}/output"));
    let stderr = name(&format!("{DIR}/stderr"));
    let root_output = name(&format!("{DIR}/root-output"));
    assert_eq!(
        effects,
        [
            Effect::Truncate { file: output.clone(), len: 0, tag: Tag(RUN.0 + CUT) },
            Effect::Truncate { file: stderr.clone(), len: 0, tag: Tag(RUN.0 + CUT + 1) },
            Effect::Truncate { file: root_output.clone(), len: 0, tag: Tag(RUN.0 + CUT + 2) },
        ]
    );
    effects.clear();

    // `output` is cut: its bytes next.
    assert!(!writing.step(RUN, done(CUT, Done::Truncated), &mut effects).unwrap());
    let printed = b"cargo:rustc-cfg=has_z\n".to_vec();
    assert_eq!(effects, [Effect::WriteAt { file: output.clone(), at: 0, bytes: printed, tag: Tag(RUN.0 + BYTES) }]);
    effects.clear();
    // An empty `stderr` is done once cut.
    assert!(!writing.step(RUN, done(CUT + 1, Done::Truncated), &mut effects).unwrap());
    assert!(effects.is_empty());
    assert!(!writing.step(RUN, done(CUT + 2, Done::Truncated), &mut effects).unwrap());
    let out_dir = b"/w/target/debug/build/z-sys-0123456789abcdef/out".to_vec();
    assert_eq!(effects, [Effect::WriteAt { file: root_output, at: 0, bytes: out_dir, tag: Tag(RUN.0 + BYTES + 2) }]);
    effects.clear();

    // `output` is dated only once it has all its bytes.
    assert!(!writing.step(RUN, done(BYTES, Done::Written), &mut effects).unwrap());
    assert_eq!(effects, [Effect::Times { file: output, modified_ns: BEGAN_NS, tag: Tag(RUN.0 + TIMED) }]);
    effects.clear();
    assert!(!writing.step(RUN, done(BYTES + 2, Done::Written), &mut effects).unwrap());
    assert!(writing.step(RUN, done(TIMED, Done::Timed), &mut effects).unwrap());
    assert!(effects.is_empty());
}

#[test]
fn an_empty_output_is_dated_once_cut() {
    let unit = unit();
    let mut effects = Vec::new();
    let mut writing = Writing::begin(&unit, Vec::new(), b"note\n".to_vec(), began(), RUN, &mut effects).unwrap();
    effects.clear();
    assert!(!writing.step(RUN, done(CUT, Done::Truncated), &mut effects).unwrap());
    let output = name(&format!("{DIR}/output"));
    assert_eq!(effects, [Effect::Times { file: output, modified_ns: BEGAN_NS, tag: Tag(RUN.0 + TIMED) }]);
}

#[test]
fn a_write_that_fails_names_its_file() {
    let unit = unit();
    let mut effects = Vec::new();
    let mut writing = Writing::begin(&unit, Vec::new(), Vec::new(), began(), RUN, &mut effects).unwrap();
    let failed = Done::Failed(IoError("No space left on device".to_owned()));
    let error = writing.step(RUN, done(CUT + 1, failed), &mut effects).unwrap_err();
    assert_eq!(error, format!("writing {DIR}/stderr: No space left on device"));
}

#[test]
fn a_completion_of_a_tag_it_never_asked_for_is_an_error() {
    let unit = unit();
    let mut effects = Vec::new();
    let mut writing = Writing::begin(&unit, Vec::new(), Vec::new(), began(), RUN, &mut effects).unwrap();
    for own in [0, TIMED + 1] {
        let error = writing.step(RUN, done(own, Done::Truncated), &mut effects).unwrap_err();
        assert!(error.contains("never asked for"), "{error}");
    }
}

#[test]
fn a_run_that_began_before_the_epoch_is_refused_naming_output() {
    let unit = unit();
    let mut effects = Vec::new();
    let before = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
    let Err(error) = Writing::begin(&unit, Vec::new(), Vec::new(), before, RUN, &mut effects) else {
        panic!("a time before the epoch was taken");
    };
    assert!(error.starts_with(&format!("{DIR}/output: the run began at")), "{error}");
    assert!(effects.is_empty());
}

#[test]
fn a_file_is_named_under_the_drivers_root() {
    assert_eq!(file_name(std::path::Path::new("/w/target/x/output")).unwrap(), name("w/target/x/output"));
    assert!(file_name(std::path::Path::new("/")).is_err());
}

#[test]
fn a_directory_the_run_left_is_no_file_of_it() {
    let files = vec![
        OutFile { path: "gen".to_owned(), executable: false, bytes: None },
        OutFile { path: "gen/z.rs".to_owned(), executable: false, bytes: Some(b"pub const Z: u8 = 1;".to_vec()) },
    ];
    assert_eq!(out_files(files), [("gen/z.rs".to_owned(), b"pub const Z: u8 = 1;".to_vec())]);
}

/// A stamp of the live record at `size` bytes.
fn record(size: u64) -> Done {
    Done::Stamped(vec![Stamp { ino: 7, size, mtime_ns: size, ctime_ns: size, mode: 0o100644 }])
}

/// One look of `quiet`: its timer fired, then its stamp told as `stamped`. What it asked next,
/// and what it found.
fn look(quiet: &mut Quiet, stamped: Done) -> (Vec<Effect>, Option<Stuck>) {
    let mut effects = Vec::new();
    assert_eq!(quiet.step(RUN, done(TICK, Done::Fired), &mut effects), Ok(None));
    assert!(matches!(&effects[..], [Effect::Stamp { tag, .. }] if *tag == Tag(RUN.0 + LOOK)), "{effects:?}");
    effects.clear();
    let found = quiet.step(RUN, done(LOOK, stamped), &mut effects).unwrap();
    (effects, found)
}

#[test]
fn a_run_whose_live_record_stays_as_it_is_is_stuck_and_one_that_reads_is_not() {
    let mut effects = Vec::new();
    let mut quiet = Quiet::begin(&name(&format!("{DIR}/blaze-live")), RUN, &mut effects);
    assert!(matches!(&effects[..], [Effect::Timer { after, tag }] if *after == LOOK_EVERY && *tag == Tag(RUN.0 + TICK)));
    assert!(Quiet::owns(RUN, Tag(RUN.0 + TICK)) && Quiet::owns(RUN, Tag(RUN.0 + LOOK)));
    assert!(!Quiet::owns(RUN, RUN) && !Quiet::owns(RUN, Tag(RUN.0 + TIMED)), "the spawn's and cargo's files' are not");

    let looks = (QUIET_MOST.as_secs() / LOOK_EVERY.as_secs()) as u64;
    for size in 1..=looks * 2 {
        let (next, found) = look(&mut quiet, record(size));
        assert_eq!(found, None, "a record that grows is a run making progress");
        assert!(matches!(&next[..], [Effect::Timer { .. }]), "{next:?}");
    }
    let held = looks * 2;
    for at in 1..looks {
        assert_eq!(look(&mut quiet, record(held)).1, None, "quiet for {at} looks, short of the bound");
    }
    let (next, found) = look(&mut quiet, record(held));
    assert!(next.is_empty(), "nothing more is asked of a run past its bound");
    assert_eq!(found, Some(Stuck { quiet: QUIET_MOST, ran: LOOK_EVERY * (held + looks) as u32 }));
}

#[test]
fn a_look_that_fails_or_ends_otherwise_is_an_error() {
    let mut effects = Vec::new();
    let mut quiet = Quiet::begin(&name("w/blaze-live"), RUN, &mut effects);
    let failed = Done::Failed(IoError("Permission denied".to_owned()));
    let error = quiet.step(RUN, done(LOOK, failed), &mut effects).unwrap_err();
    assert!(error.contains("Permission denied"), "{error}");
    assert!(quiet.step(RUN, done(LOOK, Done::Stamped(Vec::new())), &mut effects).unwrap_err().contains("no file"));
    assert!(quiet.step(RUN, done(TICK, Done::Truncated), &mut effects).unwrap_err().contains("Truncated"));
}
