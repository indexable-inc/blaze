//! THE SIMULATION: drawn sequences of C-level calls (open, read, lseek, dup, close, chdir,
//! getenv, setenv) drive the [`Sys`] machine over the real wire against a model host (a few
//! files under `/pkg`, a few variables), and an oracle written apart from the layer checks:
//!
//! - a read gives exactly the model file's bytes from the description's offset, which `dup`
//!   shares;
//! - a relative path is the working directory joined (`chdir` included), resolved lexically;
//! - each variable crosses as `Env` at most once, and a variable the run set is answered from
//!   the run's own value without crossing.
//!
//! Controls plant the four bugs of plant.rs. The table's order and the `mmap` rule are checked
//! apart.

use core::fmt;
use std::{format, string::String, vec, vec::Vec as StdVec};

use blaze_comptime::wire::{read_call, split_frame, write_answer};
use blaze_comptime::{Answer, Call, FileKind, GuestEffect, GuestEvent, Nanos, SysError, Want, WireLimits, Words};
use blaze_own::Alloc;
use blaze_own::planted::Planted;
use blaze_sim::{Broken, Config, Tape, check, controls};

use crate::{Errno, Fd, Limits, Os, Ret, Sys, SysOpts, SysPlant, SysState};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const FILES: [(&str, &[u8]); 3] = [("/pkg/a.txt", b"alpha bytes"), ("/pkg/src/lib.rs", b"fn f() {}\n"), ("/pkg/src/b.rs", b"")];
const DIRS: [&str; 2] = ["/pkg", "/pkg/src"];
const SPELLED: [&str; 8] = ["a.txt", "src/lib.rs", "lib.rs", "./b.rs", "src/../a.txt", "/pkg/src/lib.rs", "missing", "../a.txt"];
const VARS: [(&str, Option<&str>); 3] = [("HOME", Some("/home")), ("OUT_DIR", None), ("TARGET", Some("aarch64"))];

#[derive(Clone, Debug)]
enum Op {
    Open(usize),
    Read { fd: usize, len: usize },
    Seek { fd: usize, to: i64 },
    Dup(usize),
    Close(usize),
    Chdir(usize),
    Getenv(usize),
    Setenv(usize),
}

#[derive(Clone, Debug)]
enum SysBroken {
    Open { path: String, expected: bool, got: String },
    Read { fd: i32, expected: StdVec<u8>, got: StdVec<u8> },
    Env { name: &'static str, expected: Option<String>, got: Option<String> },
    EnvAsked { name: String, times: usize },
}

impl fmt::Display for SysBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl SysPlant breaks SysBroken {
        None,
        CwdIgnored => SysBroken::Open { .. },
        ReadPastEnd => SysBroken::Read { .. },
        DupCopies => SysBroken::Read { .. },
        EnvAskedTwice => SysBroken::EnvAsked { .. } | SysBroken::Env { .. },
    }
}

fn harness(what: impl fmt::Debug) -> Broken<SysBroken> {
    Broken::Harness(format!("{what:?}"))
}

/// The model host: answers from FILES, DIRS and VARS, counting `Env` asks.
#[derive(Default)]
struct Host {
    env_asks: StdVec<String>,
}

impl Host {
    fn answer(&mut self, body: &[u8]) -> Result<StdVec<u8>, Broken<SysBroken>> {
        let call = read_call(body, WireLimits::SHIPPED).map_err(harness)?;
        let listing: StdVec<u8>;
        let answer = match call {
            Call::Hello { protocol } => Answer::Hello { protocol },
            Call::Cwd => Answer::Value { value: Some(&b"/pkg"[..]) },
            Call::Args => Answer::Words { words: Words::new(b"prog\0").ok_or_else(|| harness("words"))? },
            Call::Look { path, want } => {
                let file = FILES.iter().find(|(p, _)| p.as_bytes() == path);
                let dir = DIRS.iter().any(|d| d.as_bytes() == path);
                match (want, file, dir) {
                    (Want::Read, Some((_, bytes)), _) => Answer::File { bytes },
                    (Want::Stat, Some((_, bytes)), _) => Answer::Stat { kind: FileKind::File, len: bytes.len() as u64 },
                    (Want::Read, None, true) => Answer::Error(SysError::IsADir),
                    (Want::Stat, None, true) => Answer::Stat { kind: FileKind::Dir, len: 0 },
                    (Want::List, None, true) => {
                        let mut names = StdVec::new();
                        let prefix = format!("{}/", core::str::from_utf8(path).map_err(harness)?);
                        for (p, _) in FILES {
                            if let Some(rest) = p.strip_prefix(prefix.as_str()) && !rest.contains('/') {
                                names.extend_from_slice(rest.as_bytes());
                                names.push(0);
                            }
                        }
                        listing = names;
                        Answer::Listing { names: Words::new(&listing).ok_or_else(|| harness("listing"))? }
                    }
                    (Want::List, Some(_), _) => Answer::Error(SysError::NotADir),
                    _ => Answer::Error(SysError::NotFound),
                }
            }
            Call::Env { name } => {
                let name = String::from_utf8_lossy(name).into_owned();
                let value = VARS.iter().find(|(n, _)| *n == name).and_then(|(_, v)| *v);
                self.env_asks.push(name);
                Answer::Value { value: value.map(str::as_bytes) }
            }
            _ => Answer::Done,
        };
        let mut out = blaze_own::Vec::new_in(heap());
        write_answer(&answer, &mut out, WireLimits::SHIPPED).map_err(harness)?;
        let (body, _) = split_frame(&out, WireLimits::SHIPPED).map_err(harness)?.ok_or_else(|| harness("no frame"))?;
        Ok(body.to_vec())
    }
}

/// Drives the call in flight (or the start) to its end.
fn drive<P: Planted<SysPlant>>(sys: &mut Sys<'_, P>, host: &mut Host) -> Result<Option<Result<Ret, Errno>>, Broken<SysBroken>> {
    loop {
        match sys.state() {
            SysState::Done => return Ok(Some(sys.result().map_err(harness)?)),
            SysState::Ready => return Ok(None),
            SysState::Starting | SysState::Asking => {
                let frame = match sys.next_effect() {
                    Some(GuestEffect::Send { frame }) => frame.to_vec(),
                    None => return Err(harness("asking, with nothing to send")),
                };
                let (body, _) = split_frame(&frame, WireLimits::SHIPPED).map_err(harness)?.ok_or_else(|| harness("no frame"))?;
                let reply = host.answer(body)?;
                sys.step(Nanos(0), GuestEvent::Frame(&reply)).map_err(harness)?;
            }
            state => return Err(harness(state)),
        }
    }
}

fn int(ret: Option<Result<Ret, Errno>>) -> Result<i64, Errno> {
    match ret {
        Some(Ok(Ret::Int(n))) => Ok(n),
        Some(Ok(_)) => Ok(0),
        Some(Err(errno)) => Err(errno),
        None => Err(Errno::Io),
    }
}

/// The model's lexical join, written apart from sys.rs.
fn join(cwd: &str, spelled: &str) -> String {
    let full = if spelled.starts_with('/') { String::from(spelled) } else { format!("{cwd}/{spelled}") };
    let mut parts: StdVec<&str> = StdVec::new();
    for part in full.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// One open description in the model.
struct Desc {
    bytes: &'static [u8],
    at: usize,
}

fn case(tape: &Tape, plant: impl Planted<SysPlant>) -> Result<(), Broken<SysBroken>> {
    let ops = tape.vec(0..=20, "ops", |t| match t.index(8, "op") {
        0 => Op::Open(t.index(SPELLED.len(), "path")),
        1 => Op::Read { fd: t.index(6, "fd"), len: t.int_in(0usize..=8, "len") },
        2 => Op::Seek { fd: t.index(6, "fd"), to: t.int_in(0i64..=12, "to") },
        3 => Op::Dup(t.index(6, "fd")),
        4 => Op::Close(t.index(6, "fd")),
        5 => Op::Chdir(t.index(DIRS.len(), "dir")),
        6 => Op::Getenv(t.index(VARS.len(), "var")),
        _ => Op::Setenv(t.index(VARS.len(), "var")),
    });
    let mut host = Host::default();
    let mut sys = Sys::new_planted(SysOpts { os: Os::Mac, limits: Limits { page: 16384, .. }, alloc: heap(), .. }, plant);
    drive(&mut sys, &mut host)?;
    // The model: descriptor numbers to descriptions (shared by dup), the cwd, the overlay.
    let mut fds: StdVec<Option<usize>> = vec![None, None, None];
    let mut descs: StdVec<Desc> = StdVec::new();
    let mut cwd = String::from("/pkg");
    let mut overlay: StdVec<(&'static str, &'static str)> = StdVec::new();
    for op in ops {
        match op {
            Op::Open(at) => {
                let path = join(&cwd, SPELLED[at]);
                let file = FILES.iter().find(|(p, _)| *p == path);
                sys.open(-2, SPELLED[at].as_bytes(), 0).map_err(harness)?;
                let got = int(drive(&mut sys, &mut host)?);
                match (file, got) {
                    (Some((_, bytes)), Ok(fd)) => {
                        while fds.len() <= fd as usize {
                            fds.push(None);
                        }
                        descs.push(Desc { bytes, at: 0 });
                        fds[fd as usize] = Some(descs.len() - 1);
                    }
                    // A directory opens too (a stub): the model tracks no bytes for it.
                    (None, Ok(fd)) if DIRS.contains(&path.as_str()) => {
                        let _ = sys.close(Fd(fd as i32));
                    }
                    (None, Err(Errno::NoEnt)) => {}
                    (file, got) => {
                        return Err(Broken::Oracle(SysBroken::Open { path, expected: file.is_some(), got: format!("{got:?}") }));
                    }
                }
            }
            Op::Read { fd, len } => {
                let mut buf = vec![0u8; len];
                let got = sys.read(Fd(fd as i32), &mut buf, None);
                match fds.get(fd).copied().flatten() {
                    Some(at) => {
                        let desc = &mut descs[at];
                        let start = desc.at.min(desc.bytes.len());
                        let end = (start + len).min(desc.bytes.len());
                        let expected = desc.bytes[start..end].to_vec();
                        desc.at = end;
                        let got = got.map(|n| buf[..n].to_vec()).unwrap_or_default();
                        if got != expected {
                            return Err(Broken::Oracle(SysBroken::Read { fd: fd as i32, expected, got }));
                        }
                    }
                    None if fd < 3 => {}
                    None => {
                        if got != Err(Errno::BadF) {
                            return Err(harness(format!("read of closed {fd}: {got:?}")));
                        }
                    }
                }
            }
            Op::Seek { fd, to } => {
                let got = sys.lseek(Fd(fd as i32), to, 0);
                if let Some(at) = fds.get(fd).copied().flatten() {
                    descs[at].at = to as usize;
                    if got != Ok(to as u64) {
                        return Err(harness(format!("lseek {fd} {to}: {got:?}")));
                    }
                }
            }
            Op::Dup(fd) => {
                if let Some(at) = fds.get(fd).copied().flatten() {
                    let new = sys.dup(Fd(fd as i32), 0).map_err(harness)?.0 as usize;
                    while fds.len() <= new {
                        fds.push(None);
                    }
                    fds[new] = Some(at);
                }
            }
            Op::Close(fd) => {
                if fd >= 3 && fds.get(fd).copied().flatten().is_some() {
                    sys.close(Fd(fd as i32)).map_err(harness)?;
                    fds[fd] = None;
                }
            }
            Op::Chdir(at) => {
                sys.chdir(DIRS[at].as_bytes()).map_err(harness)?;
                int(drive(&mut sys, &mut host)?).map_err(harness)?;
                cwd = String::from(DIRS[at]);
            }
            Op::Getenv(at) => {
                let name = VARS[at].0;
                sys.getenv(name.as_bytes()).map_err(harness)?;
                let got = match drive(&mut sys, &mut host)? {
                    Some(Ok(Ret::Env(Some(value)))) => {
                        let value = sys.env_value(value).ok_or_else(|| harness("env value"))?;
                        Some(String::from_utf8_lossy(&value[..value.len() - 1]).into_owned())
                    }
                    Some(Ok(Ret::Env(None))) => None,
                    other => return Err(harness(other)),
                };
                let expected = match overlay.iter().rev().find(|(n, _)| *n == name) {
                    Some((_, value)) => Some(String::from(*value)),
                    None => VARS[at].1.map(String::from),
                };
                if got != expected {
                    return Err(Broken::Oracle(SysBroken::Env { name, expected, got }));
                }
            }
            Op::Setenv(at) => {
                let name = VARS[at].0;
                sys.setenv(name.as_bytes(), Some(b"set-by-run"), true).map_err(harness)?;
                overlay.push((name, "set-by-run"));
            }
        }
    }
    for (name, _) in VARS {
        let times = host.env_asks.iter().filter(|asked| asked.as_str() == name).count();
        if times > 1 {
            return Err(Broken::Oracle(SysBroken::EnvAsked { name: String::from(name), times }));
        }
    }
    Ok(())
}

#[test]
fn the_layer_answers_reads_paths_and_variables_as_the_model_does() {
    check(Config::cases(3000), |tape| case(tape, blaze_own::planted::NoPlant));
}

#[test]
fn every_layer_plant_is_caught_by_its_invariant() {
    controls(Config::cases(3000), |tape, plant: SysPlant| case(tape, plant));
}

#[test]
fn the_table_is_sorted_and_found_by_name() {
    for pair in crate::NAMES.windows(2) {
        assert!(pair[0].c.as_bytes() < pair[1].c.as_bytes(), "{} before {}", pair[0].c, pair[1].c);
    }
    assert!(crate::find("open", Os::Mac).is_ok());
    assert!(matches!(crate::find("ptrace", Os::Linux), Err(crate::BindRefused { why: crate::Unbound::NotInTable, .. })));
    assert!(matches!(crate::find("statx", Os::Mac), Err(crate::BindRefused { why: crate::Unbound::OtherSystem { .. }, .. })));
}

#[test]
fn only_anonymous_private_memory_without_execution_is_mapped() {
    let (mac, linux) = (crate::MAC, crate::LINUX);
    let anon = |c: crate::Consts| c.map_private | c.map_anon;
    assert_eq!(crate::mmap_allowed(Os::Mac, 3, anon(mac), -1), Ok(()));
    assert_eq!(crate::mmap_allowed(Os::Linux, 0, anon(linux) | 0x20000, -1), Ok(()));
    assert_eq!(crate::mmap_allowed(Os::Mac, 7, anon(mac), -1), Err(Errno::Perm));
    assert_eq!(crate::mmap_allowed(Os::Mac, 3, anon(mac) | mac.map_fixed, -1), Err(Errno::Perm));
    assert_eq!(crate::mmap_allowed(Os::Mac, 3, anon(mac) | 0x800, -1), Err(Errno::Perm));
    assert_eq!(crate::mmap_allowed(Os::Linux, 1, linux.map_private, 3), Err(Errno::Perm));
    assert_eq!(crate::mmap_allowed(Os::Linux, 3, linux.map_shared | linux.map_anon, -1), Err(Errno::Perm));
    assert_eq!(crate::mprotect_allowed(4), Err(Errno::Perm));
}

#[test]
fn emfile_does_not_leak_held_bytes() {
    let mut host = Host::default();
    // `most_held = 22` allows the cap check (`11 + 11 <= 22`) to pass so `install` hits EMFILE.
    let limits = Limits { most_fds: 4, most_file: 64, most_held: 22, page: 16384, .. };
    let mut sys = Sys::new(SysOpts { os: Os::Mac, limits, alloc: heap(), .. });
    drive(&mut sys, &mut host).unwrap();

    // fd 3 takes 11 bytes (`/pkg/a.txt`).
    sys.open(-2, b"a.txt", 0).unwrap();
    let fd3 = int(drive(&mut sys, &mut host).unwrap()).unwrap();
    assert_eq!(fd3, 3);

    // Repeated opens hit EMFILE (fds 0..4 are full); if `held` leaked on the first EMFILE,
    // the second open would fail with EFBIG instead of EMFILE.
    for _ in 0..3 {
        sys.open(-2, b"a.txt", 0).unwrap();
        assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Err(Errno::MFile));
    }

    // Close fd 3: held bytes must be 0, so reopening fits cleanly.
    sys.close(Fd(fd3 as i32)).unwrap();
    sys.open(-2, b"a.txt", 0).unwrap();
    let reopened = int(drive(&mut sys, &mut host).unwrap()).unwrap();
    assert_eq!(reopened, 3);
}

#[test]
fn out_write_ftruncate_and_pipe_enforce_caps_and_compact() {
    let mut host = Host::default();
    let limits = Limits { most_file: 16, most_held: 20, page: 16384, .. };
    let mut sys = Sys::new(SysOpts { os: Os::Mac, limits, alloc: heap(), .. });
    drive(&mut sys, &mut host).unwrap();

    // Open an output file via O_CREAT | O_TRUNC | O_RDWR.
    let c = crate::MAC;
    sys.open(c.at_fdcwd, b"/out/f", c.o_rdwr | c.o_creat | c.o_trunc).unwrap();
    let fd = Fd(int(drive(&mut sys, &mut host).unwrap()).unwrap() as i32);

    // Checked overflow on write offset returns EFBIG.
    sys.write(fd, b"abc", Some(u64::MAX - 1)).unwrap();
    assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Err(Errno::FBig));

    // Exceeding `most_file` on write and ftruncate returns EFBIG.
    sys.write(fd, &[1u8; 17], None).unwrap();
    assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Err(Errno::FBig));
    sys.ftruncate(fd, 17).unwrap();
    assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Err(Errno::FBig));

    // Write 12 bytes to file (held = 12).
    sys.write(fd, &[7u8; 12], None).unwrap();
    assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Ok(12));

    // Pipe write respects `most_held` (12 + 9 > 20 -> EFBIG; 12 + 8 <= 20 -> Ok).
    let (pr, pw) = sys.pipe().unwrap();
    sys.write(pw, &[2u8; 9], None).unwrap();
    assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Err(Errno::FBig));

    // Repeated write + read of 8 bytes compacts consumed prefix and releases `held` back to 12.
    let mut buf = [0u8; 8];
    for i in 0u8..10 {
        sys.write(pw, &[i; 8], None).unwrap();
        assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Ok(8));
        assert_eq!(sys.read(pr, &mut buf, None), Ok(8));
        assert_eq!(buf, [i; 8]);
    }

    // Buffer 8 bytes in pipe (held = 20), then close read end -> releases pipe buffer (held = 12).
    sys.write(pw, &[9u8; 8], None).unwrap();
    assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Ok(8));
    sys.close(pr).unwrap();
    // Now ftruncate(fd, 16) fits within most_held (16 <= 20)!
    sys.ftruncate(fd, 16).unwrap();
    assert_eq!(int(drive(&mut sys, &mut host).unwrap()), Ok(0));
    sys.close(pw).unwrap();
    sys.close(fd).unwrap();
}

#[test]
fn setenv_updates_in_place_respects_overwrite_false_and_invalidates_environ() {
    struct EnvHost {
        env_all_calls: usize,
    }
    impl EnvHost {
        fn run(&mut self, sys: &mut Sys<'_>) -> Option<Result<Ret, Errno>> {
            loop {
                match sys.state() {
                    SysState::Done => return Some(sys.result().unwrap()),
                    SysState::Ready => return None,
                    SysState::Starting | SysState::Asking => {
                        let frame = match sys.next_effect().unwrap() {
                            GuestEffect::Send { frame } => frame.to_vec(),
                        };
                        let (body, _) = split_frame(&frame, WireLimits::SHIPPED).unwrap().unwrap();
                        let call = read_call(body, WireLimits::SHIPPED).unwrap();
                        let answer = match call {
                            Call::Hello { protocol } => Answer::Hello { protocol },
                            Call::Cwd => Answer::Value { value: Some(b"/pkg") },
                            Call::EnvAll => {
                                self.env_all_calls += 1;
                                Answer::Words { words: Words::new(b"HOME=/home\0KEEP=1\0").unwrap() }
                            }
                            _ => Answer::Done,
                        };
                        let mut out = blaze_own::Vec::new_in(heap());
                        write_answer(&answer, &mut out, WireLimits::SHIPPED).unwrap();
                        let (reply, _) = split_frame(&out, WireLimits::SHIPPED).unwrap().unwrap();
                        sys.step(Nanos(0), GuestEvent::Frame(reply)).unwrap();
                    }
                    state => panic!("unexpected state {state:?}"),
                }
            }
        }
    }

    let mut host = EnvHost { env_all_calls: 0 };
    let limits = Limits { most_env: 2, page: 16384, .. };
    let mut sys = Sys::new(SysOpts { os: Os::Mac, limits, alloc: heap(), .. });
    host.run(&mut sys);

    // Fetch environ first so `env_all` is populated.
    sys.environ().unwrap();
    host.run(&mut sys).unwrap().unwrap();
    assert_eq!(host.env_all_calls, 1);
    assert!(sys.environ_words().is_some());

    // `overwrite = false` respects a variable already present in `env_all`.
    sys.setenv(b"HOME", Some(b"/other"), false).unwrap();
    assert!(sys.environ_words().is_some(), "no-op setenv must not invalidate env_all");

    // Repeated `setenv` on the same key updates in place and never exhausts `most_env = 2`.
    for i in 0..20 {
        let val = format!("v{i}");
        sys.setenv(b"K", Some(val.as_bytes()), true).unwrap();
    }
    assert!(sys.environ_words().is_none(), "mutating setenv must invalidate env_all");

    // `overwrite = false` does not overwrite an existing overlay variable.
    sys.setenv(b"K", Some(b"ignored"), false).unwrap();
    sys.getenv(b"K").unwrap();
    let Ret::Env(Some(at)) = host.run(&mut sys).unwrap().unwrap() else { panic!() };
    assert_eq!(sys.env_value(at), Some(&b"v19\0"[..]));

    // Unset `HOME`, re-fetch `environ()`, and verify merged words contain `KEEP=1`, `K=v19`, not `HOME`.
    sys.setenv(b"HOME", None, true).unwrap();
    sys.environ().unwrap();
    host.run(&mut sys).unwrap().unwrap();
    assert_eq!(host.env_all_calls, 2);
    let words: StdVec<&[u8]> = sys.environ_words().unwrap().split_inclusive(|b| *b == 0).collect();
    assert!(words.contains(&&b"KEEP=1\0"[..]));
    assert!(words.contains(&&b"K=v19\0"[..]));
    assert!(!words.iter().any(|w| w.starts_with(b"HOME=")));
}

#[test]
fn readdir_is_o1_and_rewinddir_resets_cursor() {
    let mut listing = StdVec::new();
    for i in 0..500u32 {
        listing.extend_from_slice(format!("entry_{i:04}").as_bytes());
        listing.push(0);
    }

    let mut sys = Sys::new(SysOpts { os: Os::Mac, limits: Limits { page: 16384, .. }, alloc: heap(), .. });
    let drive_list = |sys: &mut Sys<'_>| -> Option<Result<Ret, Errno>> {
        loop {
            match sys.state() {
                SysState::Done => return Some(sys.result().unwrap()),
                SysState::Ready => return None,
                SysState::Starting | SysState::Asking => {
                    let frame = match sys.next_effect().unwrap() {
                        GuestEffect::Send { frame } => frame.to_vec(),
                    };
                    let (body, _) = split_frame(&frame, WireLimits::SHIPPED).unwrap().unwrap();
                    let call = read_call(body, WireLimits::SHIPPED).unwrap();
                    let answer = match call {
                        Call::Hello { protocol } => Answer::Hello { protocol },
                        Call::Cwd => Answer::Value { value: Some(b"/pkg") },
                        Call::Look { want: Want::List, .. } => Answer::Listing { names: Words::new(&listing).unwrap() },
                        _ => Answer::Done,
                    };
                    let mut out = blaze_own::Vec::new_in(heap());
                    write_answer(&answer, &mut out, WireLimits::SHIPPED).unwrap();
                    let (reply, _) = split_frame(&out, WireLimits::SHIPPED).unwrap().unwrap();
                    sys.step(Nanos(0), GuestEvent::Frame(reply)).unwrap();
                }
                state => panic!("unexpected {state:?}"),
            }
        }
    };
    drive_list(&mut sys);

    sys.opendir(b"/pkg").unwrap();
    let fd = Fd(int(drive_list(&mut sys)).unwrap() as i32);

    let read_all = |sys: &mut Sys<'_>| -> StdVec<(StdVec<u8>, u64, u64)> {
        let mut out = StdVec::new();
        while let Some(ent) = sys.readdir(fd).unwrap() {
            assert_ne!(ent.ino, 0);
            out.push((ent.name.to_vec(), ent.ino, ent.at));
        }
        out
    };
    let first = read_all(&mut sys);
    assert_eq!(first.len(), 502);
    assert_eq!(first[0].0, b".\0");
    assert_eq!(first[1].0, b"..\0");
    assert_eq!(first[2].0, b"entry_0000\0");
    assert_eq!(first[501].0, b"entry_0499\0");
    assert_eq!(first[501].2, 501);

    sys.rewinddir(fd).unwrap();
    let second = read_all(&mut sys);
    assert_eq!(first, second);
    sys.close(fd).unwrap();
}

#[test]
fn open_trunc_uses_stat_and_single_rtt_setlen() {
    let mut seen_calls: StdVec<String> = StdVec::new();
    let mut sys = Sys::new(SysOpts { os: Os::Mac, limits: Limits { page: 16384, .. }, alloc: heap(), .. });
    let pump = |sys: &mut Sys<'_>, seen_calls: &mut StdVec<String>| -> Option<Result<Ret, Errno>> {
        loop {
            match sys.state() {
                SysState::Done => return Some(sys.result().unwrap()),
                SysState::Ready => return None,
                SysState::Starting | SysState::Asking => {
                    let frame = match sys.next_effect().unwrap() {
                        GuestEffect::Send { frame } => frame.to_vec(),
                    };
                    let (body, _) = split_frame(&frame, WireLimits::SHIPPED).unwrap().unwrap();
                    let call = read_call(body, WireLimits::SHIPPED).unwrap();
                    let answer = match call {
                        Call::Hello { protocol } => Answer::Hello { protocol },
                        Call::Cwd => Answer::Value { value: Some(b"/pkg") },
                        Call::Look { path, want } => {
                            seen_calls.push(format!("Look({:?}, {want:?})", core::str::from_utf8(path).unwrap()));
                            Answer::Stat { kind: FileKind::File, len: 1024 }
                        }
                        Call::SetLen { path, len } => {
                            seen_calls.push(format!("SetLen({:?}, {len})", core::str::from_utf8(path).unwrap()));
                            Answer::Done
                        }
                        Call::WriteFile { path, .. } => {
                            seen_calls.push(format!("WriteFile({:?})", core::str::from_utf8(path).unwrap()));
                            Answer::Done
                        }
                        _ => Answer::Done,
                    };
                    let mut out = blaze_own::Vec::new_in(heap());
                    write_answer(&answer, &mut out, WireLimits::SHIPPED).unwrap();
                    let (reply, _) = split_frame(&out, WireLimits::SHIPPED).unwrap().unwrap();
                    sys.step(Nanos(0), GuestEvent::Frame(reply)).unwrap();
                }
                state => panic!("unexpected {state:?}"),
            }
        }
    };
    pump(&mut sys, &mut seen_calls);

    let c = crate::MAC;
    // O_WRONLY | O_CREAT | O_TRUNC collapses to a single SetLen(0) round trip.
    seen_calls.clear();
    sys.open(c.at_fdcwd, b"/out/a", c.o_wronly | c.o_creat | c.o_trunc).unwrap();
    let fd1 = Fd(int(pump(&mut sys, &mut seen_calls)).unwrap() as i32);
    assert_eq!(seen_calls, vec!["SetLen(\"/out/a\", 0)"]);

    // O_WRONLY | O_TRUNC (without O_CREAT) uses Want::Stat, never Want::Read, then SetLen(0).
    seen_calls.clear();
    sys.open(c.at_fdcwd, b"/out/b", c.o_wronly | c.o_trunc).unwrap();
    let fd2 = Fd(int(pump(&mut sys, &mut seen_calls)).unwrap() as i32);
    assert_eq!(seen_calls, vec!["Look(\"/out/b\", Stat)", "SetLen(\"/out/b\", 0)"]);

    sys.close(fd1).unwrap();
    sys.close(fd2).unwrap();
}

#[test]
fn actions_destroy_reclaims_slot() {
    let mut sys = Sys::new(SysOpts { os: Os::Mac, limits: Limits { page: 16384, .. }, alloc: heap(), .. });
    let a = sys.actions_new();
    sys.actions_dup2(a, 1, 2).unwrap();
    sys.actions_chdir(a, b"/pkg").unwrap();
    sys.actions_destroy(a).unwrap();
    assert_eq!(sys.actions_destroy(a), Err(Errno::Inval));
    assert_eq!(sys.actions_dup2(a, 1, 2), Err(Errno::Inval));
    assert_eq!(sys.actions_new(), a, "freed actions slot must be reused");
}
