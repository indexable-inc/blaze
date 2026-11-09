//! A build script's run (#1450, #805, #1480, #804, #1296): what it is given, what its result
//! is keyed by, and what cargo's rule watches of it.
//!
//! - **Given**: the script's code (the content ID of the binary the daemon compiled it to, so
//!   a script rebuilt to the same bytes is the same script), its directory, and each variable
//!   of its environment that blaze sets from the unit. A variable passed through from the
//!   environment the build was started with (`PATH`, `CC`, `HOME`) is not given: what the
//!   script reads of those is traced, so a `PATH` that differs only where the script never
//!   looks keeps its result (#805). Which is which is [`blaze_build_script::Env::origin`].
//! - **Traced**: every file, listing and variable the script and every program it ran read,
//!   the C compiler and its headers among them (#1480). The variables passed through are lent
//!   to it, not given ([`crate::confine::GIVEN`]): its environment holds the given ones
//!   alone, and what it reads of the others by name is traced by name, so a variable it never
//!   reads (a shell's `OLDPWD` or `SHLVL`) is in no key. A script that walks its environment
//!   (`env::vars`, a `Command` that passes it on to a program) is lent all of it then, and
//!   read all of it.
//! - **Portable** (docs/v2/plan/design/store.md, "Portable keys"): every path and value in
//!   the key and in what is traced is written against the run's roots ([`crate::roots`]): its
//!   package, its `OUT_DIR`, the target directory, the workspace, blaze's tools and launcher
//!   ([`Place::roots`]), so the same script of the same package in another checkout is found
//!   (not on Windows, where a run has no roots).
//! - The jobserver's variable ([`RUN_ONLY`], `CARGO_MAKEFLAGS`) is of this run alone, as it is
//!   not in cargo's fingerprint: neither given nor traced. make's own ([`MAKE_FLAGS`]:
//!   `MAKEFLAGS`, `MFLAGS`, passed through from a build started under make) carry its flags
//!   and its variable overrides (`make CFLAGS=-O3`) as well, which a script that runs make
//!   honours: they are traced as any variable passed through, by their value without the
//!   jobserver's parts ([`make_flags`]).
//! - **Elided** (#1296): a script that provably only prints constant cargo directives is not
//!   compiled or run; what it prints is known from its source ([`inert`]). In verify mode it
//!   is run as well, and the two must agree ([`verify_elided`]).

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use blaze_build_script::{Error, Unit, output};
use blaze_core::{Command, Exit, Sandbox, Traced};
use blaze_ids::ContentId;

use crate::{
    Given, RunKey, RunKind,
    confine::GIVEN,
    roots::{Role, RoleSet, RootMap},
    rule::CargoRule,
    run::{self, Mode, OutFile, Value},
    trace::ALL_ENV_NAME,
};

/// Variables of one run alone: a script's jobserver, which another run is given another of.
pub const RUN_ONLY: &[&str] = &["CARGO_MAKEFLAGS"];

/// make's variables, passed through from a build started under make: its flags and variable
/// overrides, which a script that runs make honours, and the jobserver of make's run, which is
/// of that run alone. What is traced and checked of them is their value without the
/// jobserver's parts ([`make_flags`]).
pub const MAKE_FLAGS: &[&str] = &["MAKEFLAGS", "MFLAGS"];

/// What a script's run needs that the unit doesn't say.
#[derive(Debug)]
pub struct Place {
    /// The content ID of the script's binary.
    pub code: ContentId,
    /// What it may read besides what every sandbox reads: its package, the toolchain.
    pub read: Vec<PathBuf>,
    /// The pinned native inputs it is given (docs/cargo.md, "Pinned native inputs"): each the
    /// variable it reads the layout from and the archive's digest (`sha256:<hex>`), sorted by
    /// variable. Their digests are a part of its key, so another archive is another run
    /// whatever directory it is laid out in; empty adds no part, and leaves every other
    /// script's key as it was.
    pub pinned: Vec<(String, String)>,
    /// Whether its package's manifest lets it reach the network: then what it reached is
    /// untraceable, and it is kept by cargo's rule.
    pub network: bool,
    pub memory: Option<u64>,
    pub mode: Mode,
    /// The roots the build has besides the run's own package and `OUT_DIR`, which
    /// [`prepare`] adds: the target directory, the workspace, blaze's tools and its launcher.
    pub roots: RootMap,
}

/// A script's run, ready to begin, and the environment a check of its inputs is made in.
#[derive(Debug)]
pub struct Prepared {
    pub spec: run::Spec,
    /// The environment a check of its inputs sees, the one [`crate::inputs::Disk`] answers it
    /// with: what the script sees but the names given ([`Prepared::given`], whose values the
    /// key has) and of the run alone ([`RUN_ONLY`]), sorted by name and each once, as
    /// [`crate::inputs::Env`] keeps it. What [`keyed`] leaves of a trace is what this
    /// environment gives, so a run that read some of it by name, or the whole of it (one that
    /// walked it, or passed it on whole to a program it started), is found again under the
    /// key it was kept under.
    pub env: Vec<(OsString, OsString)>,
    /// The names given, which are left out of what is traced: the key has them already.
    pub given: Vec<OsString>,
    /// What the key was made of: `made.finish()` is `spec.key.given`.
    pub made: Given,
}

impl Prepared {
    /// The script's whole environment, as it is started in it (its command's), given names
    /// and make's jobserver too: what a pure key's variables have the values of, on keep as on
    /// find ([`script_env`]). A pure key has no given part, so [`Prepared::env`], which leaves
    /// the given names out, would keep each of them as not set and never be found again.
    pub fn script_env(&self) -> &[(OsString, OsString)] {
        // The launcher's [`GIVEN`], which [`prepare`] adds after the script's (and the daemon
        // adds the launcher's live record after it), is no variable of the script's.
        let env = &self.spec.command.env;
        &env[..env.iter().position(|it| it.0 == GIVEN).unwrap_or(env.len())]
    }
}

/// The environment `unit`'s script sees, started in `inherited`: what [`prepare`] starts its
/// run in ([`Prepared::script_env`]), and what a pure key's find reads its variables from.
pub fn script_env(
    unit: &Unit,
    inherited: &[(OsString, OsString)],
) -> Result<Vec<(OsString, OsString)>, Error> {
    Ok(blaze_build_script::env(unit, inherited)?.vars)
}

/// `unit`'s run, in a build started with `inherited` (cargo's environment once started:
/// [`blaze_build_script::env::cargo_started`]).
pub fn prepare(
    unit: &Unit,
    inherited: &[(OsString, OsString)],
    place: &Place,
) -> Result<Prepared, Error> {
    let env = blaze_build_script::env(unit, inherited)?;
    let mut roots = place
        .roots
        .clone() // clone: setup, the run's roots are the build's and its own
        .with(Role::Package, &unit.package.manifest_dir)
        .with(Role::OutDir, &unit.out_dir);
    let view = roots.view();
    let mut given = Given::new(RunKind::BuildScript);
    // The roots a given string still holds once ported (`-L/ws/p/lib`, a root after a path
    // byte): its key is then this checkout's string, and the run is kept here alone.
    let mut bound = RoleSet::default();
    let mut names = Vec::new(); // alloc: setup, the names the check leaves out
    for (name, value) in env.set_from_unit().filter(|(name, _)| !run_only(name)) {
        bound = bound.union(view.left(value.as_encoded_bytes()));
        given.env(name.to_owned(), view.os(value).into_owned()); // alloc: setup, the given variables
        names.push(name.to_owned()); // alloc: setup, as above
    }
    given.roots(&view);
    // Its binary is `place.code`, by content; its path is what it sees as `argv[0]`. On Windows
    // `os_bytes` is UTF-16 units and a run has no roots, so these stay absolute there.
    let program = crate::trace::os_bytes(unit.script.as_os_str());
    let dir = crate::trace::os_bytes(unit.package.manifest_dir.as_os_str());
    given.ported("program", &program, &view);
    given.ported("dir", &dir, &view);
    if !place.pinned.is_empty() {
        let mut pinned: Vec<&(String, String)> = place.pinned.iter().collect(); // alloc: setup, a run's few inputs
        pinned.sort();
        let mut bytes = Vec::new(); // alloc: setup, as above
        for (name, digest) in pinned {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(digest.as_bytes());
            bytes.push(0);
        }
        given.part("pinned native inputs", &bytes);
    }
    bound = bound.union(view.left(&program)).union(view.left(&dir));
    let key = RunKey { code: place.code, given: given.finish() };
    roots.bind(bound);
    let vars: Vec<(OsString, OsString)> = env.vars.into_iter().collect(); // alloc: setup, the script's environment
    let checked = checked_env(&vars, &names);
    // The script's environment, and last the launcher's [`GIVEN`]: the names its key has,
    // which the program's environment holds alone; every other variable is lent to it and
    // traced as it is read.
    let mut command_env = Vec::with_capacity(vars.len() + 1); // alloc: setup, the command's environment
    command_env.extend(vars.iter().cloned()); // clone: setup, as above; the check keeps its own
    command_env.push((
        GIVEN.into(),
        crate::confine::given_value(names.iter().map(OsString::as_os_str)).map_err(Error)?,
    ));
    let command = Command {
        program: unit.script.clone(), // clone: setup, the command owns its program
        args: Vec::new(),             // alloc: none, an empty list
        env: command_env,
        dir: unit.package.manifest_dir.clone(), // clone: setup, the command owns its directory
        stdin: Vec::new(),                      // alloc: none, empty
        sandbox: Sandbox::On {
            read: place.read.clone(), // clone: setup, the sandbox owns what it may read
            write: vec![unit.out_dir.clone()], // clone: setup, the sandbox owns what it may write
            network: place.network,
            trace: true,
            services: Vec::new(), // alloc: none, an empty list
        },
        memory: place.memory,
    };
    // clone: setup, the run owns its OUT_DIR
    let spec =
        run::Spec { key, command, out_dir: Some(unit.out_dir.clone()), mode: place.mode, roots };
    Ok(Prepared { spec, env: checked, given: names, made: given })
}

/// `vars`, the script's environment, as a check of its inputs sees it ([`Prepared::env`]):
/// without the names `given` and of the run alone, sorted by name, each name once, make's
/// variables without their jobserver ([`make_flags`]).
fn checked_env(vars: &[(OsString, OsString)], given: &[OsString]) -> Vec<(OsString, OsString)> {
    let mut checked: Vec<(OsString, OsString)> = vars
        .iter()
        .filter(|(name, _)| !run_only(name) && !given.contains(name))
        .cloned() // clone: setup, the check's environment, once per run prepared
        .collect(); // alloc: as above
    // As `inputs::Env::new` keeps it: sorted stably by name, the first of a name kept. The
    // first is the one the sandbox's trace has too, so make's are stripped after.
    checked.sort_by(|a, b| a.0.cmp(&b.0));
    checked.dedup_by(|later, first| later.0 == first.0);
    checked.retain_mut(|(name, value)| match make_flag(name) {
        true => match make_flags(value) {
            Some(kept) => {
                *value = kept;
                true
            }
            None => false,
        },
        false => true,
    });
    checked
}

fn run_only(name: &OsStr) -> bool {
    RUN_ONLY.iter().any(|it| name == *it)
}

fn make_flag(name: &OsStr) -> bool {
    MAKE_FLAGS.iter().any(|it| name == *it)
}

/// `value`, of one of [`MAKE_FLAGS`], without what names one run's jobserver: the words `-j`,
/// `-j<N>`, `--jobs`, `--jobs=<N>`, `--jobserver-auth=..` and `--jobserver-fds=..`, which
/// cargo's fingerprint ignores as well. What follows the word `--` is make's variable
/// overrides, kept as they are. `None` when nothing else is left: a build started under make
/// with no flag of its own is as one started outside make. A value that is not UTF-8 is kept
/// whole (make writes text; one that is not is then keyed with its jobserver, which only misses).
pub fn make_flags(value: &OsStr) -> Option<OsString> {
    let Some(text) = value.to_str() else { return Some(value.to_owned()) }; // alloc: setup, once per run prepared
    let mut kept: Vec<&str> = Vec::new(); // alloc: setup, once per run prepared under make
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if rest.is_empty() {
            break;
        }
        // A word ends at whitespace that is not escaped (make writes `\ ` for a space in one).
        let mut escaped = false;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                let ends = !escaped && c.is_ascii_whitespace();
                escaped = !escaped && c == '\\';
                ends
            })
            .map_or(rest.len(), |(at, _)| at);
        let (word, after) = rest.split_at(end);
        if word == "--" {
            kept.push(rest);
            break;
        }
        if !jobserver(word) {
            kept.push(word);
        }
        rest = after;
    }
    (!kept.is_empty()).then(|| kept.join(" ").into()) // alloc: setup, as above
}

/// Whether `word` of make's flags is of its jobserver: how many jobs, and where their tokens are.
fn jobserver(word: &str) -> bool {
    let jobs = |count: &str| count.bytes().all(|it| it.is_ascii_digit());
    word.strip_prefix("-j").is_some_and(jobs)
        || word == "--jobs"
        || word.strip_prefix("--jobs=").is_some_and(jobs)
        || word.starts_with("--jobserver-auth=")
        || word.starts_with("--jobserver-fds=")
}

/// A variable's value as an error may show it: whether it is set and its length, never the
/// value, which can be a secret passed through to the script.
fn shown(value: Option<&OsStr>) -> String {
    // alloc: error path
    value.map_or_else(|| "unset".to_owned(), |it| format!("a value of {} bytes", it.len()))
}

/// What is traced of a run of `prepared`, without what its key has already and what is of
/// that run alone: what the run's result is kept under. The whole environment, when it was
/// read, is the one a check sees ([`Prepared::env`]): its entries are what is left once the
/// given names and those of the run alone are taken out, and its names' hash
/// ([`ALL_ENV_NAME`]) is made again over them, so a check that hashes [`Prepared::env`] whole
/// makes the same key (rule 21: one input, one value, on both sides). make's variables are
/// kept by their value without the jobserver ([`make_flags`]), as the check has them.
///
/// What is left must be what [`Prepared::env`] says, each variable read by its value there
/// (or not set) and, when the whole environment was read, every one of it: a trace that holds
/// another value, or misses one, would be kept under a key no check makes again, and is an
/// error naming the variable rather than a run that is never found.
pub fn keyed(prepared: &Prepared, traced: &mut Traced) -> Result<(), Error> {
    traced.env.retain(|(name, _)| !run_only(name) && !prepared.given.contains(name));
    for (name, value) in &mut traced.env {
        if make_flag(name) {
            *value = value.take().and_then(|it| make_flags(&it));
        }
    }
    let whole = traced.env.iter().any(|it| it.0 == ALL_ENV_NAME);
    if whole {
        // A make variable left with nothing is not set, as the check's environment has it:
        // what read the whole environment did not read it.
        traced.env.retain(|(name, value)| value.is_some() || !make_flag(name));
    }
    let checked = |name: &OsStr| {
        prepared
            .env
            .binary_search_by(|it| it.0.as_os_str().cmp(name))
            .ok()
            .map(|at| prepared.env[at].1.as_os_str())
    };
    for (name, value) in traced.env.iter().filter(|it| it.0 != ALL_ENV_NAME) {
        if value.as_deref() != checked(name) {
            // alloc: error path
            return Err(Error(format!(
                "{} was traced reading `{}` as {}, and a check of it reads {}: its \
                 result would be kept under a key no check makes (rule 21, a bug in blaze; \
                 the values are not shown, as a variable can hold a secret)",
                prepared.spec.command.program.display(),
                name.display(),
                shown(value.as_deref()),
                shown(checked(name))
            )));
        }
    }
    if whole {
        // About a hundred names on each side, once per run kept: no order of the trace assumed.
        let missing =
            prepared.env.iter().find(|(name, _)| !traced.env.iter().any(|it| it.0 == *name));
        if let Some((name, _)) = missing {
            // alloc: error path
            return Err(Error(format!(
                "{} read its whole environment, and its trace has no `{}`, which a check of \
                 it reads: its result would be kept under a key no check makes (rule 21, a bug in blaze)",
                prepared.spec.command.program.display(),
                name.display()
            )));
        }
        if let Some(all) = traced.env.iter_mut().find(|it| it.0 == ALL_ENV_NAME) {
            all.1 = Some(crate::trace::names_hash(&prepared.env).into());
        }
    }
    Ok(())
}

/// What cargo watches of `unit`'s run that ended as `exit` did: its `rerun-if-changed` and
/// `rerun-if-env-changed`, or its whole package when it named no path. Output cargo refuses
/// is the error cargo gives, and the run is not kept.
pub fn cargo_rule(
    unit: &Unit,
    inherited: &[(OsString, OsString)],
    exit: &Exit,
) -> Result<CargoRule, Error> {
    let cx = output::Context::of(unit, inherited);
    blaze_build_script::watched_in(&exit.stdout, &cx, &unit.package.manifest_dir).map(CargoRule::of)
}

/// What the script whose root source file is `root` would have been as a run, when its source
/// (`files`, by path) proves it only prints constant cargo directives and `flags`, what blaze
/// compiles it with, can't fail that compile: it is then neither compiled nor run.
pub fn inert(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>, flags: &[String]) -> Option<Value> {
    if !blaze_build_script::elide::flags_allow(flags) {
        return None;
    }
    blaze_build_script::elide(root, files).map(elided)
}

/// The result of a script that prints `stdout` and does nothing else: what a run of it would
/// have been, with an empty `OUT_DIR`.
pub fn elided(stdout: Vec<u8>) -> Value {
    // alloc: none, empty lists
    Value { code: Some(0), signal: None, stdout, stderr: Vec::new(), files: Vec::<OutFile>::new() }
}

/// Whether the run of a script [`inert`] read agrees with what it said, for verify mode.
pub fn verify_elided(elided: &Value, ran: &Value) -> Result<(), Error> {
    blaze_build_script::verify(&elided.stdout, &ran.stdout)?;
    match run::differs(elided, ran) {
        // alloc: error path
        Some(what) => Err(Error(format!(
            "the elided build script's run differs from what elision said: {what}"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> Unit {
        let mut unit = Unit::default();
        unit.package.name = "p".into();
        unit.package.version = "0.1.0".into();
        unit.package.manifest_dir = "/ws/p".into();
        unit.package.manifest_path = "/ws/p/Cargo.toml".into();
        unit.tools.cargo = "/bin/cargo".into();
        unit.tools.rustc = "/bin/rustc".into();
        unit.tools.rustdoc = "/bin/rustdoc".into();
        unit.profile.opt_level = "0".into();
        unit.script = "/t/build/p/script".into();
        unit.out_dir = "/t/build/p/0123456789abcdef/out".into();
        unit.target = "x86_64-unknown-linux-gnu".into();
        unit.host = "x86_64-unknown-linux-gnu".into();
        unit
    }

    fn place() -> Place {
        Place {
            code: ContentId::of("blaze v2 test 1", b"script"),
            read: vec!["/ws/p".into()],
            pinned: Vec::new(),
            network: false,
            memory: None,
            mode: Mode::Normal,
            roots: RootMap::default()
                .with(Role::Target, Path::new("/t"))
                .with(Role::Tools, Path::new("/bin")),
        }
    }

    /// `unit()` in a checkout at `home`.
    #[cfg(not(windows))]
    fn moved(home: &str) -> (Unit, Place) {
        let mut unit = unit();
        let at = |path: &Path| PathBuf::from(format!("{home}{}", path.display()));
        unit.package.manifest_dir = at(&unit.package.manifest_dir);
        unit.package.manifest_path = at(&unit.package.manifest_path);
        unit.tools.cargo = at(&unit.tools.cargo);
        unit.tools.rustc = at(&unit.tools.rustc);
        unit.tools.rustdoc = at(&unit.tools.rustdoc);
        unit.script = at(&unit.script);
        unit.out_dir = at(&unit.out_dir);
        let mut place = place();
        place.roots = RootMap::default()
            .with(Role::Target, &at(Path::new("/t")))
            .with(Role::Workspace, &at(Path::new("/ws")))
            .with(Role::Tools, &at(Path::new("/bin")));
        (unit, place)
    }

    #[cfg(not(windows))] // Windows has no roots (`blaze_usercode::roots`).
    #[test]
    fn two_checkouts_at_different_paths_give_one_key() {
        let (a, a_place) = moved("/home/a");
        let (b, b_place) = moved("/srv/ci/b");
        let one = prepare(&a, &inherited("/a"), &a_place).unwrap();
        let two = prepare(&b, &inherited("/a"), &b_place).unwrap();
        assert_eq!(one.spec.key, two.spec.key);
        // What it is given is still its checkout's.
        assert_ne!(one.spec.command.env, two.spec.command.env);
        // Without its roots, a checkout elsewhere is another key.
        let (mut c, mut c_place) = moved("/srv/ci/c");
        c_place.roots = RootMap::default();
        assert_ne!(one.spec.key, prepare(&c, &inherited("/a"), &c_place).unwrap().spec.key);
        // A path given that is under no root stays absolute.
        c_place = moved("/srv/ci/c").1;
        c.tools.rustc = "/opt/rustc".into();
        assert_ne!(one.spec.key, prepare(&c, &inherited("/a"), &c_place).unwrap().spec.key);
    }

    fn inherited(path: &str) -> Vec<(OsString, OsString)> {
        vec![("PATH".into(), path.into()), ("HOME".into(), "/home/u".into())]
    }

    /// A pinned native input's digest is part of the run's key: another archive is another
    /// run, even laid out at the same place; and a script given none keeps the key it had.
    #[test]
    fn the_key_has_each_pinned_inputs_digest() {
        let mut given = unit();
        given.config_env.push(blaze_build_script::unit::ConfigEnv {
            name: "LLVM_SYS_231_PREFIX".into(),
            value: "/home/u/.blaze/native/llvm".into(),
            force: true,
        });
        let with = |digest: &str| {
            let place = Place {
                pinned: vec![("LLVM_SYS_231_PREFIX".into(), format!("sha256:{digest}"))],
                ..place()
            };
            prepare(&given, &inherited("/a"), &place).unwrap().spec.key
        };
        let (a, b) = (with(&"a".repeat(64)), with(&"b".repeat(64)));
        assert_ne!(a, b);
        assert_eq!(a, with(&"a".repeat(64)));
        let none = prepare(&given, &inherited("/a"), &place()).unwrap().spec.key;
        assert_ne!(a, none);
        // The variable it is given is set from the unit: in the key by its value too.
        assert!(
            prepare(&given, &inherited("/a"), &place())
                .unwrap()
                .given
                .iter()
                .any(|it| it == "LLVM_SYS_231_PREFIX")
        );
    }

    #[test]
    fn a_path_passed_through_is_traced_not_given() {
        let one = prepare(&unit(), &inherited("/a"), &place()).unwrap();
        let two = prepare(&unit(), &inherited("/b"), &place()).unwrap();
        assert_eq!(one.spec.key, two.spec.key);
        assert!(!one.given.iter().any(|it| it == "PATH"));
        assert!(one.given.iter().any(|it| it == "OUT_DIR"));
        let mut other = unit();
        other.target = "aarch64-unknown-linux-gnu".into();
        let three = prepare(&other, &inherited("/a"), &place()).unwrap();
        assert_ne!(one.spec.key, three.spec.key);
        for prepared in [&one, &two, &three] {
            assert_eq!(
                prepared.made.finish(),
                prepared.spec.key.given,
                "the key is made of what is kept"
            );
        }
        let Sandbox::On { write, trace, .. } = &one.spec.command.sandbox else { panic!() };
        assert!(*trace && write == &[unit().out_dir]);
    }

    #[test]
    fn what_is_given_or_of_one_run_is_not_traced() {
        let prepared = prepare(&unit(), &inherited("/a"), &place()).unwrap();
        let mut traced = Traced {
            env: vec![
                ("OUT_DIR".into(), Some("/t/build/p/0123456789abcdef/out".into())),
                ("CARGO_MAKEFLAGS".into(), Some("-j --jobserver-auth=fifo:/tmp/x".into())),
                ("CC".into(), None),
            ],
            ..Traced::default()
        };
        keyed(&prepared, &mut traced).unwrap();
        assert_eq!(traced.env, vec![("CC".into(), None)]);
    }

    /// A trace that says another value than the check's environment has, or misses a variable
    /// of it when the whole environment was read, is refused naming the variable: kept, it
    /// would be under a key no check makes, and the run would never be found again.
    #[test]
    fn a_trace_that_disagrees_with_the_check_is_refused_naming_the_variable() {
        let prepared = prepare(&unit(), &inherited("/a"), &place()).unwrap();
        let mut other =
            Traced { env: vec![("PATH".into(), Some("/elsewhere".into()))], ..Traced::default() };
        let error = keyed(&prepared, &mut other).unwrap_err();
        assert!(error.0.contains("`PATH`") && error.0.contains("a value of 10 bytes"), "{error:?}");
        // A value can be a secret: the error names its length, never its bytes.
        assert!(!error.0.contains("/elsewhere"), "{error:?}");
        // The whole environment read, every variable of the check's there but `HOME`.
        let mut env = vec![(OsString::from(ALL_ENV_NAME), Some(OsString::from("h")))];
        env.extend(
            prepared
                .env
                .iter()
                .filter(|it| it.0 != "HOME")
                .map(|(name, value)| (name.clone(), Some(value.clone()))),
        );
        let mut short = Traced { env, ..Traced::default() };
        let error = keyed(&prepared, &mut short).unwrap_err();
        assert!(error.0.contains("no `HOME`"), "{error:?}");
    }

    #[test]
    fn make_flags_are_kept_without_their_jobserver() {
        let kept = |value: &str| make_flags(OsStr::new(value)).map(|it| it.into_string().unwrap());
        assert_eq!(kept("-j8 --jobserver-auth=3,4"), None);
        assert_eq!(kept(" -j --jobserver-fds=3,4 -j"), None);
        assert_eq!(kept("--jobs=4 --jobserver-auth=fifo:/tmp/GMfifo1"), None);
        assert_eq!(kept("k -j8 --jobserver-auth=3,4"), Some("k".into()));
        assert_eq!(
            kept("k -j8 --jobserver-auth=3,4 -- CFLAGS=-O3"),
            Some("k -- CFLAGS=-O3".into())
        );
        // Overrides are kept as they are, a `-j` among them too; an escaped space is in its word.
        assert_eq!(kept(" -- CFLAGS=-O3\\ -j2  X=1"), Some("-- CFLAGS=-O3\\ -j2  X=1".into()));
        assert_eq!(kept("-I/a\\ b -j3"), Some("-I/a\\ b".into()));
        assert_eq!(kept("-jx"), Some("-jx".into()));
    }

    /// make's variables carry its variable overrides (`make CFLAGS=-O3`), which a script that
    /// runs make honours: they are traced and checked by their value without the jobserver, so
    /// another jobserver finds the run kept and another override does not. A script that
    /// names one in `rerun-if-env-changed` has that value recorded.
    #[cfg(not(windows))] // Windows has no roots, and the check here goes through them.
    #[test]
    fn make_flags_are_inputs_without_their_jobserver() {
        use crate::{inputs::Disk, rule};

        let make = |flags: &str| {
            let mut env = inherited("/a");
            env.push(("MAKEFLAGS".into(), flags.into()));
            env.push(("CARGO_MAKEFLAGS".into(), flags.into()));
            prepare(&unit(), &env, &place()).unwrap()
        };
        let first = make("k -j8 --jobserver-auth=3,4 -- CFLAGS=-O3");
        assert!(
            first.env.iter().any(|it| it.0 == "MAKEFLAGS" && it.1 == "k -- CFLAGS=-O3"),
            "{:?}",
            first.env
        );
        assert!(!first.env.iter().any(|it| it.0 == "CARGO_MAKEFLAGS"));
        let store = kept_whole(&first);
        assert!(
            finds(&store, &make("k -j4 --jobserver-auth=5,6 -- CFLAGS=-O3")),
            "another jobserver misses"
        );
        assert!(
            !finds(&store, &make("k -j8 --jobserver-auth=3,4 -- CFLAGS=-O0")),
            "another override finds"
        );
        assert!(
            !finds(&store, &make("-j8 --jobserver-auth=3,4")),
            "no override finds the run given one"
        );
        // Only the jobserver: as a build outside make.
        let outside = kept_whole(&prepare(&unit(), &inherited("/a"), &place()).unwrap());
        assert!(finds(&outside, &make("-j8 --jobserver-auth=3,4")));
        // Cargo's rule records what the check has of it.
        let mut traced =
            Traced { untraceable: vec![blaze_core::Untraceable::Clock], ..Traced::default() };
        let watched = CargoRule {
            env: vec!["MAKEFLAGS".into(), "CARGO_MAKEFLAGS".into()],
            ..CargoRule::default()
        };
        rule::widen(&mut traced, &watched, &mut Disk::new(first.env.clone())).unwrap();
        assert_eq!(
            traced.env,
            vec![
                ("CARGO_MAKEFLAGS".into(), None),
                ("MAKEFLAGS".into(), Some("k -- CFLAGS=-O3".into()))
            ]
        );
    }

    /// The store a run of `prepared` that read its whole environment leaves, as `Run::complete`
    /// keeps it: its shape in the run's first slot, and its result.
    #[cfg(not(windows))]
    fn kept_whole(prepared: &Prepared) -> Vec<(blaze_cas::Key, Vec<u8>)> {
        kept_reading(prepared, &[crate::trace::ALL_ENV])
    }

    /// The store a run of `prepared` that read the variables `names` leaves (`*`: the whole
    /// environment), as `Run::complete` keeps it.
    #[cfg(not(windows))]
    fn kept_reading(prepared: &Prepared, names: &[&str]) -> Vec<(blaze_cas::Key, Vec<u8>)> {
        use crate::{
            index::{self, Shape},
            inputs::Disk,
            trace::{self, Line, Seen},
        };

        // The run, as the sandbox reports it: what it read of the environment it was started with.
        let lines: Vec<Line> = names.iter().map(|name| Line::new(Seen::Env, "", name)).collect();
        let mut traced =
            trace::assemble(&lines, None, &mut Disk::new(prepared.script_env().to_vec())).unwrap();
        keyed(prepared, &mut traced).unwrap();
        let roots = prepared.spec.roots.view();
        let (marked, stored) =
            roots.kept(&traced, &elided(b"cargo:rustc-cfg=x\n".to_vec())).unwrap();
        vec![
            (index::slot(&prepared.spec.key, 0), Shape::of(&marked.local).encode()),
            (index::result_key(&prepared.spec.key, &marked.local), stored.encode()),
        ]
    }

    /// The key rule for what a script is lent (rule 21, keys cover every input): its key
    /// holds the variables it read and no other. Two shells that differ only in variables the
    /// run never read (`OLDPWD`, `SHLVL`, the shell's temporary directory) find the run one
    /// kept; one whose value of a variable it read differs does not. The program's environment
    /// holds the given names alone ([`GIVEN`]), so nothing it was not lent by a read it was
    /// seen to make reaches it.
    #[cfg(not(windows))] // Windows has no roots, and the check here goes through them.
    #[test]
    fn a_variable_passed_through_is_in_the_key_only_when_it_was_read() {
        let shell = |path: &str, oldpwd: &str, shlvl: &str| {
            let mut env = inherited(path);
            env.push(("OLDPWD".into(), oldpwd.into()));
            env.push(("SHLVL".into(), shlvl.into()));
            env.push(("NIX_BUILD_TOP".into(), format!("/tmp/nix-shell.{oldpwd}").into()));
            prepare(&unit(), &env, &place()).unwrap()
        };
        let first = shell("/a", "x", "1");
        let given = first
            .spec
            .command
            .env
            .iter()
            .find(|it| it.0 == GIVEN)
            .expect("the launcher is told the given names");
        let given: Vec<&str> = given.1.to_str().unwrap().split('=').collect();
        assert!(given.contains(&"OUT_DIR") && given.contains(&"TARGET"), "{given:?}");
        assert!(
            !given.iter().any(|it| ["PATH", "OLDPWD", "SHLVL", "CARGO_MAKEFLAGS"].contains(it)),
            "{given:?}"
        );
        assert!(
            !first.script_env().iter().any(|it| it.0 == GIVEN),
            "the launcher's variable is not the script's"
        );
        // It read `PATH` by name, and `TARGET`, which its key has already.
        let store = kept_reading(&first, &["PATH", "TARGET"]);
        assert!(
            finds(&store, &shell("/a", "y", "3")),
            "another shell, the same in what the run read, misses"
        );
        assert!(
            !finds(&store, &shell("/b", "x", "1")),
            "another PATH, which the run read, finds the run of another"
        );
        // One that read `OLDPWD` is keyed by it.
        let store = kept_reading(&first, &["OLDPWD"]);
        assert!(
            !finds(&store, &shell("/a", "y", "1")),
            "another OLDPWD, which the run read, finds the run of another"
        );
        assert!(finds(&store, &shell("/b", "x", "2")));
    }

    /// Whether a check of `prepared`, as the daemon makes it, finds a run in `store`.
    #[cfg(not(windows))]
    fn finds(store: &[(blaze_cas::Key, Vec<u8>)], prepared: &Prepared) -> bool {
        use crate::{
            index::{self, Found},
            inputs::Disk,
            roots::Rooted,
        };

        let get = |key: &blaze_cas::Key| {
            Ok::<_, blaze_cas::Corrupt>(
                store.iter().find(|it| it.0 == *key).map(|it| it.1.as_slice()),
            )
        };
        let mut disk = Disk::new(prepared.env.clone());
        let found = index::search(
            get,
            &prepared.spec.key,
            &mut Rooted::new(prepared.spec.roots.view(), &mut disk),
        );
        match found {
            Ok(Found::Hit { .. }) => true,
            Ok(Found::Miss { .. }) => false,
            Err(error) => panic!("checking the run: {error:?}"),
        }
    }

    /// A pure key has no given part: its variables (what cargo gives the script, `TARGET`,
    /// `OUT_DIR`) are kept by their values, read from the script's whole environment on keep
    /// as on find. Kept from the trimmed environment a sandboxed run's own key is checked
    /// with, every given variable would be kept as not set: never found by the next build's
    /// lookup, and two runs given other values would collide on one key.
    #[cfg(not(windows))] // Windows has no roots.
    #[test]
    fn a_pure_key_kept_by_a_sandboxed_run_is_found_by_the_next_lookup() {
        use blaze_cas::{Config, Store};
        use blaze_core::{FileName, run_until_idle, sim::Faults, sim::Sim};

        use crate::{
            inputs::Disk,
            pure::{self, Proof},
        };

        let mut sim = Sim::new(11, Faults::default());
        let mut effects = Vec::new();
        let config =
            Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        }).expect("the effects end");
        assert!(store.ready());
        let proof = Proof {
            identity: ContentId::of("blaze v2 test 1", b"pure script"),
            env: vec!["OUT_DIR".into(), "TARGET".into()],
            reads: false,
            writes: false,
            prints: true,
        };
        let value = elided(b"cargo:rustc-cfg=x\n".to_vec());
        // Kept as the daemon keeps it when the sandboxed run ends: from its Start.
        let prepared = prepare(&unit(), &inherited("/a"), &place()).unwrap();
        let mut disk = Disk::new(prepared.script_env().to_vec());
        pure::keep(&mut store, &proof, prepared.spec.roots.view(), Vec::new(), &mut disk, &value)
            .unwrap();
        // The next build's lookup, before anything is compiled: the script's environment made
        // again, under the run's roots.
        let roots = place()
            .roots
            .with(Role::Package, &unit().package.manifest_dir)
            .with(Role::OutDir, &unit().out_dir);
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let find = |unit: &Unit| {
            let mut disk = Disk::new(script_env(unit, &inherited("/a")).unwrap());
            pure::find(&reader, &guard, &proof, roots.view(), &mut disk).unwrap()
        };
        assert_eq!(
            find(&unit()),
            Some(value),
            "a pure key kept by a run is not found by the next build"
        );
        let mut other = unit();
        other.target = "aarch64-unknown-linux-gnu".into();
        assert_eq!(find(&other), None, "a given variable the script reads is not in its pure key");
    }

    /// What a run that read the whole environment (every traced run: a sandbox passes it on
    /// whole to what the script starts, `$RUSTC` for `autocfg`) is kept under, as a run's
    /// completion keeps it, is what a check of the next build makes: a no-op rebuild reuses
    /// it rather than running it again. Before, the check hashed the given variables
    /// (`OUT_DIR`, `TARGET`, ...) into the key and the kept trace did not, so every build ran
    /// every script again, and a script whose `OUT_DIR` names are random (`autocfg`'s probe
    /// crates) failed with the store's mismatch.
    #[cfg(not(windows))] // Windows has no roots, and the check here goes through them.
    #[test]
    fn a_rerun_that_read_the_whole_environment_finds_the_result_it_kept() {
        let mut env = inherited("/a");
        env.push(("CARGO_MAKEFLAGS".into(), "-j8 --jobserver-auth=3,4".into()));
        let first = prepare(&unit(), &env, &place()).unwrap();
        let store = kept_whole(&first);
        // The next build: prepared again, another jobserver, checked as the daemon checks it.
        let mut again_env = inherited("/a");
        again_env.push(("CARGO_MAKEFLAGS".into(), "-j8 --jobserver-auth=5,6".into()));
        let again = prepare(&unit(), &again_env, &place()).unwrap();
        assert_eq!(again.spec.key, first.spec.key);
        assert!(finds(&store, &again), "a no-op rebuild misses its kept run");
        // A variable passed through that changed is still an input of the whole environment.
        let moved = prepare(&unit(), &inherited("/b"), &place()).unwrap();
        assert!(!finds(&store, &moved), "a changed PATH finds the run of another");
    }

    #[test]
    fn cargo_watches_what_was_named_or_the_whole_package() {
        let exit = |stdout: &[u8]| Exit {
            code: Some(0),
            signal: None,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            traced: None,
            peak_memory: 0,
        };
        let none = cargo_rule(&unit(), &inherited("/a"), &exit(b"cargo:rustc-cfg=x\n")).unwrap();
        assert_eq!(none.package, Some(PathBuf::from("/ws/p")));
        let named = cargo_rule(
            &unit(),
            &inherited("/a"),
            &exit(b"cargo:rerun-if-changed=build.rs\ncargo:rerun-if-env-changed=VERSION\n"),
        )
        .unwrap();
        assert_eq!(named.package, None);
        assert_eq!(named.paths, vec![PathBuf::from("/ws/p/build.rs")]);
        assert_eq!(named.env, vec![OsString::from("VERSION")]);
        let env_only =
            cargo_rule(&unit(), &inherited("/a"), &exit(b"cargo:rerun-if-env-changed=VERSION\n"))
                .unwrap();
        assert_eq!(env_only.package, None);
        assert!(env_only.paths.is_empty());
    }

    /// Output cargo refuses is its error, never a guess of what to watch.
    #[test]
    fn refused_output_is_an_error_not_a_whole_package_watch() {
        let exit = Exit {
            code: Some(0),
            signal: None,
            stdout: b"cargo:rustc-link-arg-bin=no-equals\n".to_vec(),
            stderr: Vec::new(),
            traced: None,
            peak_memory: 0,
        };
        let refused = cargo_rule(&unit(), &inherited("/a"), &exit).expect_err("cargo refuses it");
        assert!(refused.0.contains("rustc-link-arg-bin"), "{}", refused.0);
    }

    #[test]
    fn an_inert_script_is_read_and_checked_against_its_run() {
        let root = PathBuf::from("/ws/p/build.rs");
        let files: BTreeMap<PathBuf, Vec<u8>> =
            [(root.clone(), b"fn main() {\n    println!(\"cargo:rustc-cfg=fast\");\n}\n".to_vec())]
                .into();
        let value = inert(&root, &files, &[]).unwrap();
        assert_eq!(value.stdout, b"cargo:rustc-cfg=fast\n");
        assert!(inert(&root, &files, &["-D".into(), "warnings".into()]).is_none());
        let other: BTreeMap<PathBuf, Vec<u8>> = [(
            root.clone(),
            b"fn main() { println!(\"cargo:rustc-env=T={}\", std::env::var(\"T\").unwrap()); }\n"
                .to_vec(),
        )]
        .into();
        assert!(inert(&root, &other, &[]).is_none());
        verify_elided(&value, &elided(b"cargo:rustc-cfg=fast\n".to_vec())).unwrap();
        assert!(verify_elided(&value, &elided(b"cargo:rustc-cfg=slow\n".to_vec())).is_err());
        let mut wrote = elided(b"cargo:rustc-cfg=fast\n".to_vec());
        wrote.files.push(OutFile { path: "x".into(), executable: false, bytes: Some(Vec::new()) });
        assert!(verify_elided(&value, &wrote).is_err());
    }
}
