use std::{fs, path::PathBuf};

use base_db::Env;
use blaze_usercode::trace::{ALL_ENV, Seen};

use super::{
    Answer, Call, Os, Shim,
    files::{MANIFEST_DIR, OUT_DIR},
    target::{Errno, Field, Kind, STAT_MAX, Stat, System, Target},
};
use crate::guest::{Guest, Stop, tests::Buffer};

/// A package and an output directory beside a file neither holds, in a fresh directory of
/// its own under the system's temporary one, each path real.
struct Scratch {
    top: PathBuf,
    pkg: String,
    out: String,
    secret: String,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let top = std::env::temp_dir().join(format!("blaze-macro-eval-os-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(top.join("pkg/src")).unwrap();
        fs::create_dir_all(top.join("pkg/data")).unwrap();
        fs::create_dir_all(top.join("out")).unwrap();
        fs::write(top.join("pkg/src/lib.rs"), "pub fn f() {}\n").unwrap();
        fs::write(top.join("pkg/data/a.txt"), "hello, macro").unwrap();
        fs::write(top.join("pkg/data/b.txt"), "b").unwrap();
        fs::write(top.join("out/gen.rs"), "// generated\n").unwrap();
        fs::write(top.join("secret.txt"), "not yours").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../secret.txt", top.join("pkg/escape")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("data/a.txt", top.join("pkg/inner")).unwrap();
        let top = fs::canonicalize(&top).unwrap();
        let real = |rel: &str| top.join(rel).to_str().unwrap().to_owned();
        Scratch { pkg: real("pkg"), out: real("out"), secret: real("secret.txt"), top }
    }

    fn env(&self) -> Env {
        let mut env = Env::default();
        env.set(MANIFEST_DIR, self.pkg.clone());
        env.set(OUT_DIR, self.out.clone());
        env.set("FOO", "bar");
        env
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.top);
    }
}

fn shim(t: Target, call: Call) -> Shim {
    Shim { call, target: Some(t) }
}

/// A macro's machine over a buffer, on the host's target.
struct Run<'e> {
    os: Os<'e>,
    g: Buffer,
    t: Target,
}

impl<'e> Run<'e> {
    /// `None` on a host whose target the shims don't model, where there is nothing to test.
    fn new(env: &'e Env) -> Option<Run<'e>> {
        let t = Target::host()?;
        Some(Run { os: Os::new(env), g: Buffer { base: 0x1000, bytes: vec![0; 64], errno: None }, t })
    }

    /// Calls `call` with a return place of `ret` bytes: what it returned, sign-extended.
    fn call(&mut self, call: Call, args: &[&[u8]], ret: usize) -> Result<i64, Stop> {
        let mut out = [0u8; 16];
        let answer = self.os.call(shim(self.t, call), &mut self.g, args, &mut out[..ret])?;
        assert_eq!(answer, Answer::Answered, "{call:?} answered");
        let mut word = [0; 8];
        word[..ret.min(8)].copy_from_slice(&out[..ret.min(8)]);
        let bits = 64 - 8 * ret.clamp(1, 8) as u32;
        Ok((i64::from_le_bytes(word) << bits) >> bits)
    }

    fn c_str(&mut self, s: &str) -> u64 {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        self.g.alloc(&bytes, 1).unwrap()
    }

    fn room(&mut self, len: usize) -> u64 {
        self.g.alloc(&vec![0; len], 8).unwrap()
    }

    fn open(&mut self, path: &str, flags: i32) -> Result<i64, Stop> {
        let path = self.c_str(path);
        self.call(Call::Open, &[&path.to_le_bytes(), &flags.to_le_bytes()], 4)
    }

    fn read(&mut self, fd: i64, buf: u64, len: u64) -> i64 {
        self.call(Call::Read, &[&(fd as i32).to_le_bytes(), &buf.to_le_bytes(), &len.to_le_bytes()], 8).unwrap()
    }

    fn errno(&self, errno: Errno) -> Option<i32> {
        Some(self.t.errno(errno))
    }

    fn seen(&self) -> Vec<(Seen, String)> {
        self.os.seen().map(|(seen, what)| (seen, what.to_owned())).collect()
    }
}

#[test]
fn a_file_in_the_package_is_read_whole_and_noted() {
    let scratch = Scratch::new("read");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let path = format!("{}/data/a.txt", scratch.pkg);
    let fd = run.open(&path, 0).unwrap();
    assert_eq!(fd, 3);
    let buf = run.room(64);
    assert_eq!(run.read(fd, buf, 5), 5);
    assert_eq!(run.g.read(buf, 5).unwrap(), b"hello");
    assert_eq!(run.read(fd, buf, 64), 7);
    assert_eq!(run.g.read(buf, 7).unwrap(), b", macro");
    assert_eq!(run.read(fd, buf, 64), 0);
    assert_eq!(run.call(Call::Close, &[&(fd as i32).to_le_bytes()], 4).unwrap(), 0);
    assert_eq!(run.read(fd, buf, 1), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Badf));
    let relative = run.open("src/lib.rs", 0).unwrap();
    assert_eq!(relative, 3, "the lowest free descriptor");
    assert!(run.seen().contains(&(Seen::Read, path)));
    assert!(run.seen().contains(&(Seen::Read, format!("{}/src/lib.rs", scratch.pkg))));
    assert!(run.os.refused().next().is_none());
    assert_eq!(run.os.unkeyable(), None);
}

#[test]
fn a_file_outside_the_package_is_refused_and_not_an_input() {
    let scratch = Scratch::new("outside");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    assert_eq!(run.open(&scratch.secret, 0).unwrap(), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Acces));
    assert_eq!(run.os.refused().collect::<Vec<_>>(), [scratch.secret.as_str()]);
    assert!(run.seen().iter().all(|it| it.0 == Seen::Env), "{:?}", run.seen());
    assert!(run.open("/etc/hosts", 0).unwrap() == -1);
    let out = format!("{}/gen.rs", scratch.out);
    assert_eq!(run.open(&out, 0).unwrap(), 3, "OUT_DIR is read too");
}

#[test]
fn a_dot_dot_that_climbs_out_is_refused() {
    let scratch = Scratch::new("dotdot");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    assert_eq!(run.open("../secret.txt", 0).unwrap(), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Acces));
    assert_eq!(run.open("src/../../secret.txt", 0).unwrap(), -1);
    assert_eq!(run.os.refused().collect::<Vec<_>>(), [scratch.secret.as_str()]);
    let back = run.open("src/../data/a.txt", 0).unwrap();
    assert_eq!(back, 3, "a `..` that stays in is read");
    let seen = run.seen();
    assert!(seen.contains(&(Seen::Read, format!("{}/data/a.txt", scratch.pkg))), "{seen:?}");
    assert!(seen.contains(&(Seen::Read, format!("{}/src/../data/a.txt", scratch.pkg))), "{seen:?}");
}

#[cfg(unix)]
#[test]
fn a_link_out_of_the_package_is_refused_and_one_in_it_is_followed() {
    let scratch = Scratch::new("link");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    assert_eq!(run.open("escape", 0).unwrap(), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Acces));
    assert_eq!(run.os.refused().collect::<Vec<_>>(), [scratch.secret.as_str()]);
    let fd = run.open("inner", 0).unwrap();
    let buf = run.room(16);
    assert_eq!(run.read(fd, buf, 5), 5);
    assert_eq!(run.g.read(buf, 5).unwrap(), b"hello");
    let seen = run.seen();
    assert!(seen.contains(&(Seen::Read, format!("{}/data/a.txt", scratch.pkg))), "{seen:?}");
    assert!(seen.contains(&(Seen::Read, format!("{}/inner", scratch.pkg))), "{seen:?}");
    let buf = run.room(64);
    let path = run.c_str("inner");
    let len = run.call(Call::Readlink, &[&path.to_le_bytes(), &buf.to_le_bytes(), &64u64.to_le_bytes()], 8);
    assert_eq!(len.unwrap(), 10);
    assert_eq!(run.g.read(buf, 10).unwrap(), b"data/a.txt");
}

#[test]
fn a_missing_file_is_enoent_and_a_read_of_nothing() {
    let scratch = Scratch::new("missing");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    assert_eq!(run.open("data/none.txt", 0).unwrap(), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Noent));
    assert_eq!(run.open("gone/deeper/x.rs", 0).unwrap(), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Noent));
    let seen = run.seen();
    assert!(seen.contains(&(Seen::Read, format!("{}/data/none.txt", scratch.pkg))), "{seen:?}");
    assert!(seen.contains(&(Seen::Read, format!("{}/gone/deeper/x.rs", scratch.pkg))), "{seen:?}");
    assert_eq!(run.open("data/a.txt/x", 0).unwrap(), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Notdir));
}

#[test]
fn a_directory_is_listed_sorted_and_noted() {
    let scratch = Scratch::new("list");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let path = run.c_str("data");
    let dir = run.call(Call::Opendir, &[&path.to_le_bytes()], 8).unwrap() as u64;
    assert_ne!(dir, 0);
    let layout = run.t.dirent_layout();
    let mut names = Vec::new();
    loop {
        let entry = run.call(Call::Readdir, &[&dir.to_le_bytes()], 8).unwrap() as u64;
        if entry == 0 {
            break;
        }
        let name = run.g.read_c_str(entry + layout.name as u64, 256).unwrap();
        names.push(String::from_utf8(name.to_vec()).unwrap());
    }
    assert_eq!(names, [".", "..", "a.txt", "b.txt"]);
    let fd = run.call(Call::Dirfd, &[&dir.to_le_bytes()], 4).unwrap();
    assert_eq!(fd, 3);
    assert_eq!(run.call(Call::Closedir, &[&dir.to_le_bytes()], 4).unwrap(), 0);
    assert_eq!(run.call(Call::Readdir, &[&dir.to_le_bytes()], 8).unwrap(), 0);
    assert_eq!(run.g.errno, run.errno(Errno::Badf));
    assert!(run.seen().contains(&(Seen::Listed, format!("{}/data", scratch.pkg))));
}

#[test]
fn getenv_answers_from_the_env_given_and_notes_each_name() {
    let scratch = Scratch::new("getenv");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let foo = run.c_str("FOO");
    let value = run.call(Call::Getenv, &[&foo.to_le_bytes()], 8).unwrap() as u64;
    assert_eq!(run.g.read_c_str(value, 16).unwrap(), b"bar");
    let again = run.call(Call::Getenv, &[&foo.to_le_bytes()], 8).unwrap() as u64;
    assert_eq!(again, value, "one copy per variable");
    let nope = run.c_str("NOPE");
    assert_eq!(run.call(Call::Getenv, &[&nope.to_le_bytes()], 8).unwrap(), 0);
    assert_eq!(run.seen(), [(Seen::Env, "FOO".to_owned()), (Seen::Env, "NOPE".to_owned())]);
    let cell = run.call(Call::Environ, &[], 8).unwrap() as u64;
    let array = run.g.read_u64(cell).unwrap();
    let mut vars = Vec::new();
    for n in 0.. {
        let var = run.g.read_u64(array + 8 * n).unwrap();
        if var == 0 {
            break;
        }
        vars.push(String::from_utf8(run.g.read_c_str(var, 4096).unwrap().to_vec()).unwrap());
    }
    let mut expected: Vec<String> = env.iter().map(|(name, value)| format!("{name}={value}")).collect();
    expected.sort();
    assert_eq!(vars, expected);
    assert_eq!(run.seen().last(), Some(&(Seen::Env, ALL_ENV.to_owned())));
}

#[test]
fn setting_a_variable_stops_the_expansion() {
    let scratch = Scratch::new("setenv");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let name = run.c_str("FOO");
    let value = run.c_str("baz");
    let stop = run.call(Call::Setenv, &[&name.to_le_bytes(), &value.to_le_bytes(), &1i32.to_le_bytes()], 4);
    assert_eq!(
        stop.unwrap_err().0,
        "sets the environment variable `FOO`, which would change it for the whole compiler"
    );
    let pair = run.c_str("FOO=baz");
    let stop = run.call(Call::Putenv, &[&pair.to_le_bytes()], 4).unwrap_err();
    assert!(stop.0.starts_with("sets the environment variable `FOO`,"), "{stop}");
}

#[test]
fn opening_to_write_stops_the_expansion() {
    let scratch = Scratch::new("write");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let path = format!("{}/data/a.txt", scratch.pkg);
    let stop = run.open(&path, 1).unwrap_err();
    assert_eq!(stop.0, format!("opens `{path}` to write, which a proc macro may not"));
    let create = match run.t.system {
        System::Linux => 0o100,
        System::Apple => 0x200,
    };
    assert!(run.open("new.rs", create).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "hello, macro");
    assert!(!fs::exists(format!("{}/new.rs", scratch.pkg)).unwrap());
}

#[test]
fn the_trace_is_each_read_in_order() {
    let scratch = Scratch::new("order");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let foo = run.c_str("FOO");
    run.call(Call::Getenv, &[&foo.to_le_bytes()], 8).unwrap();
    run.open("src/lib.rs", 0).unwrap();
    run.open("data/none.txt", 0).unwrap();
    let path = run.c_str("data/b.txt");
    let buf = run.room(STAT_MAX);
    run.call(Call::Stat, &[&path.to_le_bytes(), &buf.to_le_bytes()], 4).unwrap();
    run.open("../secret.txt", 0).unwrap();
    let dir = run.c_str("data");
    let dir = run.call(Call::Opendir, &[&dir.to_le_bytes()], 8).unwrap() as u64;
    run.call(Call::Readdir, &[&dir.to_le_bytes()], 8).unwrap();
    let pkg = &scratch.pkg;
    let expected = [
        (Seen::Env, "FOO".to_owned()),
        (Seen::Env, MANIFEST_DIR.to_owned()),
        (Seen::Env, OUT_DIR.to_owned()),
        (Seen::Read, format!("{pkg}/src/lib.rs")),
        (Seen::Read, format!("{pkg}/data/none.txt")),
        (Seen::Looked, format!("{pkg}/data/b.txt")),
        (Seen::Read, format!("{pkg}/data")),
        (Seen::Listed, format!("{pkg}/data")),
    ];
    assert_eq!(run.seen(), expected);
    assert_eq!(run.os.refused().collect::<Vec<_>>(), [scratch.secret.as_str()]);
}

#[test]
fn a_stat_says_the_kind_size_and_inode_and_nothing_else() {
    let scratch = Scratch::new("stat");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let layout = run.t.stat_layout();
    let path = run.c_str("data/a.txt");
    let buf = run.room(STAT_MAX);
    assert_eq!(run.call(Call::Stat, &[&path.to_le_bytes(), &buf.to_le_bytes()], 4).unwrap(), 0);
    let field = |run: &Run<'_>, Field(at, width): Field| {
        let mut word = [0; 8];
        word[..width].copy_from_slice(run.g.read(buf + at as u64, width).unwrap());
        u64::from_le_bytes(word)
    };
    assert_eq!(field(&run, layout.st_size), 12);
    assert_eq!(field(&run, layout.mode) & 0o170000, 0o100000);
    let ino = field(&run, layout.ino);
    let fd = run.open("data/a.txt", 0).unwrap();
    let again = run.room(STAT_MAX);
    run.call(Call::Fstat, &[&(fd as i32).to_le_bytes(), &again.to_le_bytes()], 4).unwrap();
    assert_eq!(run.g.read(again, layout.size).unwrap(), run.g.read(buf, layout.size).unwrap());
    assert_ne!(ino, 0);
    let dir = run.c_str("data");
    run.call(Call::Stat, &[&dir.to_le_bytes(), &buf.to_le_bytes()], 4).unwrap();
    assert_eq!(field(&run, layout.mode) & 0o170000, 0o040000);
    let mut out = [0; STAT_MAX];
    let one = Stat { kind: Kind::File, size: 3, ino: 9 };
    assert_eq!(run.t.put_stat(&one, &mut out).len(), layout.size);
}

#[test]
fn std_s_hashmap_keys_are_fixed_and_other_randomness_is_noted() {
    let scratch = Scratch::new("random");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let mut keys = [0; 16];
    run.os.call(shim(run.t, Call::HashmapRandomKeys), &mut run.g, &[], &mut keys).unwrap();
    let mut again = [0; 16];
    run.os.call(shim(run.t, Call::HashmapRandomKeys), &mut run.g, &[], &mut again).unwrap();
    assert_eq!(keys, again);
    assert!(run.seen().is_empty());
    let buf = run.room(600);
    let len = 600u64;
    assert_eq!(run.call(Call::Getrandom, &[&buf.to_le_bytes(), &len.to_le_bytes(), &0u32.to_le_bytes()], 8).unwrap(), 600);
    assert!(run.g.read(buf, 600).unwrap().iter().any(|&it| it != 0));
    run.call(Call::Arc4randomBuf, &[&buf.to_le_bytes(), &len.to_le_bytes()], 0).unwrap();
    assert_eq!(run.seen(), [(Seen::Untraceable, "random".to_owned())]);
}

#[test]
fn the_time_of_day_is_real_and_noted_and_the_monotonic_clock_is_the_evaluators() {
    let scratch = Scratch::new("clock");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let buf = run.room(16);
    assert_eq!(run.call(Call::ClockGettime, &[&0i32.to_le_bytes(), &buf.to_le_bytes()], 4).unwrap(), 0);
    assert!(run.g.read_u64(buf).unwrap() > 1_700_000_000);
    assert_eq!(run.seen(), [(Seen::Untraceable, "clock".to_owned())]);
    let monotonic = match run.t.system {
        System::Linux => 1i32,
        System::Apple => 6,
    };
    let mut ret = [0; 4];
    let args: [&[u8]; 2] = [&monotonic.to_le_bytes(), &buf.to_le_bytes()];
    let answer = run.os.call(shim(run.t, Call::ClockGettime), &mut run.g, &args, &mut ret).unwrap();
    assert_eq!(answer, Answer::Evaluator);
}

#[test]
fn syscall_goes_to_the_shims_the_evaluator_or_stops() {
    let scratch = Scratch::new("syscall");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let mut ret = [0; 8];
    let unknown: [&[u8]; 1] = [&9999i64.to_le_bytes()];
    let stop = run.os.call(shim(run.t, Call::Syscall), &mut run.g, &unknown, &mut ret).unwrap_err();
    assert!(stop.0.starts_with("makes the system call `9999`"), "{stop}");
    if run.t.system != System::Linux {
        return;
    }
    let futex = run.t.sys_number_of_futex();
    let futex: [&[u8]; 1] = [&futex.to_le_bytes()];
    assert_eq!(run.os.call(shim(run.t, Call::Syscall), &mut run.g, &futex, &mut ret).unwrap(), Answer::Evaluator);
    let statx = run.t.sys_number_of_statx();
    let path = run.c_str("data/a.txt");
    let buf = run.room(256);
    let fdcwd = -100i32;
    let args: [&[u8]; 6] = [
        &statx.to_le_bytes(),
        &fdcwd.to_le_bytes(),
        &path.to_le_bytes(),
        &0i32.to_le_bytes(),
        &0x7ffu32.to_le_bytes(),
        &buf.to_le_bytes(),
    ];
    assert_eq!(run.call(Call::Syscall, &args, 8).unwrap(), 0);
    assert_eq!(run.g.read_u64(buf + 40).unwrap(), 12, "stx_size");
    let null = 0u64;
    let probe: [&[u8]; 6] = [&statx.to_le_bytes(), &0i32.to_le_bytes(), &null.to_le_bytes(), &0i32.to_le_bytes(), &0u32.to_le_bytes(), &null.to_le_bytes()];
    assert_eq!(run.call(Call::Syscall, &probe, 8).unwrap(), -1);
    assert_eq!(run.g.errno, run.errno(Errno::Fault), "std's probe for statx sees it is there");
}

#[test]
fn realpath_allocates_what_free_takes_back() {
    let scratch = Scratch::new("realpath");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let path = run.c_str("src/../data/a.txt");
    let real = run.call(Call::Realpath, &[&path.to_le_bytes(), &0u64.to_le_bytes()], 8).unwrap() as u64;
    let expected = format!("{}/data/a.txt", scratch.pkg);
    assert_eq!(run.g.read_c_str(real, 4096).unwrap(), expected.as_bytes());
    let mut ret = [0; 0];
    assert_eq!(run.os.call(shim(run.t, Call::Free), &mut run.g, &[&real.to_le_bytes()], &mut ret).unwrap(), Answer::Answered);
    let other = 0x1234u64;
    let stop = run.os.call(shim(run.t, Call::Free), &mut run.g, &[&other.to_le_bytes()], &mut ret).unwrap_err();
    assert!(stop.0.starts_with("frees `0x1234`"), "{stop}");
    let cwd = run.call(Call::Getcwd, &[&0u64.to_le_bytes(), &0u64.to_le_bytes()], 8).unwrap() as u64;
    assert_eq!(run.g.read_c_str(cwd, 4096).unwrap(), scratch.pkg.as_bytes());
}

/// A read once warm copies from the descriptor's buffer and allocates nothing.
#[test]
fn a_warm_read_allocates_nothing() {
    let scratch = Scratch::new("budget");
    let env = scratch.env();
    let Some(mut run) = Run::new(&env) else { return };
    let fd = run.open("data/a.txt", 0).unwrap();
    let buf = run.room(16);
    let fd = (fd as i32).to_le_bytes();
    let (buf, len, at) = (buf.to_le_bytes(), 4u64.to_le_bytes(), 0i64.to_le_bytes());
    let args: [&[u8]; 3] = [&fd, &buf, &len];
    let mut ret = [0; 8];
    run.os.call(shim(run.t, Call::Read), &mut run.g, &args, &mut ret).unwrap();
    let seek: [&[u8]; 3] = [&fd, &at, &0i32.to_le_bytes()];
    blaze_alloc_count::assert_at_most(0, "a warm read", || {
        for _ in 0..100 {
            run.os.call(shim(run.t, Call::Lseek), &mut run.g, &seek, &mut ret).unwrap();
            run.os.call(shim(run.t, Call::Read), &mut run.g, &args, &mut ret).unwrap();
        }
    });
    assert_eq!(u64::from_le_bytes(ret), 4);
}

#[test]
fn a_target_tuple_names_its_target() {
    use super::target::Arch;
    let linux = Target::of_tuple("x86_64-unknown-linux-gnu").unwrap();
    assert_eq!((linux.system, linux.arch), (System::Linux, Arch::X86_64));
    let mac = Target::of_tuple("aarch64-apple-darwin").unwrap();
    assert_eq!((mac.system, mac.arch), (System::Apple, Arch::Aarch64));
    assert_eq!(Target::of_tuple("x86_64-pc-windows-msvc"), None);
    assert_eq!(Target::of_tuple("riscv64gc-unknown-linux-gnu"), None);
    assert_eq!(mac.errno(Errno::Loop), 62);
    assert_eq!(linux.errno(Errno::Loop), 40);
}

#[test]
fn a_shim_on_a_target_not_modelled_stops() {
    let env = Env::default();
    let mut os = Os::new(&env);
    let mut g = Buffer { base: 0x1000, bytes: vec![0; 64], errno: None };
    let mut ret = [0; 8];
    let shim = Shim { call: Call::Getenv, target: None };
    assert!(os.call(shim, &mut g, &[&0x1000u64.to_le_bytes()], &mut ret).is_err());
}

#[test]
fn extern_names_are_claimed_by_link_name() {
    assert_eq!(Call::of_extern("stat$INODE64"), Some(Call::Stat));
    assert_eq!(Call::of_extern("readdir64"), Some(Call::Readdir));
    assert_eq!(Call::of_extern("close$NOCANCEL"), Some(Call::Close));
    assert_eq!(Call::of_extern("write"), None, "stdout and stderr are the evaluator's");
    assert_eq!(Call::of_extern("pthread_create"), None);
    assert_eq!(Call::of_std("hashmap_random_keys", "linux"), Some(Call::HashmapRandomKeys));
    assert_eq!(Call::of_std("environ", "unix"), Some(Call::Environ));
    assert_eq!(Call::of_std("getenv", "unix"), None);
}

/// The width of the field `field` picks of a `T`.
fn width<T, F>(_: fn(&T) -> &F) -> usize {
    size_of::<F>()
}

#[cfg(all(target_os = "linux", target_env = "gnu", target_pointer_width = "64"))]
#[test]
fn the_layouts_are_libcs() {
    use std::mem::offset_of;
    let t = Target::host().unwrap();
    let stat = t.stat_layout();
    assert_eq!(stat.size, size_of::<libc::stat64>());
    assert_eq!(stat.dev, Field(offset_of!(libc::stat64, st_dev), width::<libc::stat64, _>(|s| &s.st_dev)));
    assert_eq!(stat.ino, Field(offset_of!(libc::stat64, st_ino), width::<libc::stat64, _>(|s| &s.st_ino)));
    assert_eq!(stat.mode, Field(offset_of!(libc::stat64, st_mode), width::<libc::stat64, _>(|s| &s.st_mode)));
    assert_eq!(stat.nlink, Field(offset_of!(libc::stat64, st_nlink), width::<libc::stat64, _>(|s| &s.st_nlink)));
    assert_eq!(stat.uid, Field(offset_of!(libc::stat64, st_uid), width::<libc::stat64, _>(|s| &s.st_uid)));
    assert_eq!(stat.gid, Field(offset_of!(libc::stat64, st_gid), width::<libc::stat64, _>(|s| &s.st_gid)));
    assert_eq!(stat.rdev, Field(offset_of!(libc::stat64, st_rdev), width::<libc::stat64, _>(|s| &s.st_rdev)));
    assert_eq!(stat.st_size, Field(offset_of!(libc::stat64, st_size), width::<libc::stat64, _>(|s| &s.st_size)));
    assert_eq!(stat.blksize, Field(offset_of!(libc::stat64, st_blksize), width::<libc::stat64, _>(|s| &s.st_blksize)));
    assert_eq!(stat.blocks, Field(offset_of!(libc::stat64, st_blocks), width::<libc::stat64, _>(|s| &s.st_blocks)));
    assert_eq!(size_of::<libc::stat>(), size_of::<libc::stat64>());
    let dirent = t.dirent_layout();
    assert_eq!(dirent.size, size_of::<libc::dirent64>());
    assert_eq!(dirent.ino, Field(offset_of!(libc::dirent64, d_ino), width::<libc::dirent64, _>(|s| &s.d_ino)));
    assert_eq!(dirent.off, Field(offset_of!(libc::dirent64, d_off), width::<libc::dirent64, _>(|s| &s.d_off)));
    assert_eq!(dirent.reclen, Field(offset_of!(libc::dirent64, d_reclen), width::<libc::dirent64, _>(|s| &s.d_reclen)));
    assert_eq!(dirent.kind, Field(offset_of!(libc::dirent64, d_type), width::<libc::dirent64, _>(|s| &s.d_type)));
    assert_eq!(dirent.name, offset_of!(libc::dirent64, d_name));
    use super::target::{STATX_SIZE, statx};
    assert_eq!(STATX_SIZE, size_of::<libc::statx>());
    assert_eq!(statx::MASK, Field(offset_of!(libc::statx, stx_mask), width::<libc::statx, _>(|s| &s.stx_mask)));
    assert_eq!(statx::BLKSIZE, Field(offset_of!(libc::statx, stx_blksize), width::<libc::statx, _>(|s| &s.stx_blksize)));
    assert_eq!(statx::NLINK, Field(offset_of!(libc::statx, stx_nlink), width::<libc::statx, _>(|s| &s.stx_nlink)));
    assert_eq!(statx::UID, Field(offset_of!(libc::statx, stx_uid), width::<libc::statx, _>(|s| &s.stx_uid)));
    assert_eq!(statx::GID, Field(offset_of!(libc::statx, stx_gid), width::<libc::statx, _>(|s| &s.stx_gid)));
    assert_eq!(statx::MODE, Field(offset_of!(libc::statx, stx_mode), width::<libc::statx, _>(|s| &s.stx_mode)));
    assert_eq!(statx::INO, Field(offset_of!(libc::statx, stx_ino), width::<libc::statx, _>(|s| &s.stx_ino)));
    assert_eq!(statx::SIZE, Field(offset_of!(libc::statx, stx_size), width::<libc::statx, _>(|s| &s.stx_size)));
    assert_eq!(statx::BLOCKS, Field(offset_of!(libc::statx, stx_blocks), width::<libc::statx, _>(|s| &s.stx_blocks)));
    assert_eq!(statx::DEV_MAJOR, Field(offset_of!(libc::statx, stx_dev_major), width::<libc::statx, _>(|s| &s.stx_dev_major)));
    assert_eq!(statx::DEV_MINOR, Field(offset_of!(libc::statx, stx_dev_minor), width::<libc::statx, _>(|s| &s.stx_dev_minor)));
    let open = t.open_flags();
    assert_eq!(open.directory, i64::from(libc::O_DIRECTORY));
    assert_eq!(open.nofollow, i64::from(libc::O_NOFOLLOW));
    let writes = [libc::O_WRONLY, libc::O_RDWR, libc::O_CREAT, libc::O_TRUNC, libc::O_APPEND, libc::O_TMPFILE];
    assert!(writes.iter().all(|&it| open.writes & i64::from(it) != 0));
    assert_eq!(t.at_flags().fdcwd, i64::from(libc::AT_FDCWD));
    assert_eq!(t.at_flags().symlink_nofollow, i64::from(libc::AT_SYMLINK_NOFOLLOW));
    assert_eq!(t.at_flags().empty_path, i64::from(libc::AT_EMPTY_PATH));
    assert_eq!(t.sys_number_of_statx(), libc::SYS_statx);
    assert_eq!(t.sys_number_of_futex(), libc::SYS_futex);
    assert_errnos(t);
}

#[cfg(all(target_vendor = "apple", target_pointer_width = "64"))]
#[test]
fn the_layouts_are_libcs() {
    use std::mem::offset_of;
    let t = Target::host().unwrap();
    let stat = t.stat_layout();
    assert_eq!(stat.size, size_of::<libc::stat>());
    assert_eq!(stat.dev, Field(offset_of!(libc::stat, st_dev), width::<libc::stat, _>(|s| &s.st_dev)));
    assert_eq!(stat.ino, Field(offset_of!(libc::stat, st_ino), width::<libc::stat, _>(|s| &s.st_ino)));
    assert_eq!(stat.mode, Field(offset_of!(libc::stat, st_mode), width::<libc::stat, _>(|s| &s.st_mode)));
    assert_eq!(stat.nlink, Field(offset_of!(libc::stat, st_nlink), width::<libc::stat, _>(|s| &s.st_nlink)));
    assert_eq!(stat.uid, Field(offset_of!(libc::stat, st_uid), width::<libc::stat, _>(|s| &s.st_uid)));
    assert_eq!(stat.gid, Field(offset_of!(libc::stat, st_gid), width::<libc::stat, _>(|s| &s.st_gid)));
    assert_eq!(stat.rdev, Field(offset_of!(libc::stat, st_rdev), width::<libc::stat, _>(|s| &s.st_rdev)));
    assert_eq!(stat.st_size, Field(offset_of!(libc::stat, st_size), width::<libc::stat, _>(|s| &s.st_size)));
    assert_eq!(stat.blksize, Field(offset_of!(libc::stat, st_blksize), width::<libc::stat, _>(|s| &s.st_blksize)));
    assert_eq!(stat.blocks, Field(offset_of!(libc::stat, st_blocks), width::<libc::stat, _>(|s| &s.st_blocks)));
    let dirent = t.dirent_layout();
    assert_eq!(dirent.size, size_of::<libc::dirent>());
    assert_eq!(dirent.ino, Field(offset_of!(libc::dirent, d_ino), width::<libc::dirent, _>(|s| &s.d_ino)));
    assert_eq!(dirent.off, Field(offset_of!(libc::dirent, d_seekoff), width::<libc::dirent, _>(|s| &s.d_seekoff)));
    assert_eq!(dirent.reclen, Field(offset_of!(libc::dirent, d_reclen), width::<libc::dirent, _>(|s| &s.d_reclen)));
    let namlen = Field(offset_of!(libc::dirent, d_namlen), width::<libc::dirent, _>(|s| &s.d_namlen));
    assert_eq!(dirent.namlen, Some(namlen));
    assert_eq!(dirent.kind, Field(offset_of!(libc::dirent, d_type), width::<libc::dirent, _>(|s| &s.d_type)));
    assert_eq!(dirent.name, offset_of!(libc::dirent, d_name));
    let open = t.open_flags();
    assert_eq!(open.directory, i64::from(libc::O_DIRECTORY));
    assert_eq!(open.nofollow, i64::from(libc::O_NOFOLLOW));
    let writes = [libc::O_WRONLY, libc::O_RDWR, libc::O_CREAT, libc::O_TRUNC, libc::O_APPEND];
    assert!(writes.iter().all(|&it| open.writes & i64::from(it) != 0));
    assert_eq!(t.at_flags().fdcwd, i64::from(libc::AT_FDCWD));
    assert_eq!(t.at_flags().symlink_nofollow, i64::from(libc::AT_SYMLINK_NOFOLLOW));
    assert_errnos(t);
}

#[cfg(all(any(all(target_os = "linux", target_env = "gnu"), target_vendor = "apple"), target_pointer_width = "64"))]
fn assert_errnos(t: Target) {
    let pairs = [
        (Errno::Noent, libc::ENOENT),
        (Errno::Io, libc::EIO),
        (Errno::Badf, libc::EBADF),
        (Errno::Acces, libc::EACCES),
        (Errno::Fault, libc::EFAULT),
        (Errno::Notdir, libc::ENOTDIR),
        (Errno::Isdir, libc::EISDIR),
        (Errno::Inval, libc::EINVAL),
        (Errno::Spipe, libc::ESPIPE),
        (Errno::Rofs, libc::EROFS),
        (Errno::Range, libc::ERANGE),
        (Errno::Nametoolong, libc::ENAMETOOLONG),
        (Errno::Loop, libc::ELOOP),
    ];
    for (errno, code) in pairs {
        assert_eq!(t.errno(errno), code, "{errno:?}");
    }
}
