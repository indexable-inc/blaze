//! The sandbox on macOS. The command's own process stays outside: it makes the run's scratch
//! directory, forks the program's process, which applies a Seatbelt profile to itself
//! (`sandbox_init_with_parameters`, as Nix's and Bazel's sandboxes on macOS do) and runs the
//! program, waits for it, writes the record and removes the scratch directory.
//!
//! Seatbelt is what enforces, in the kernel and for every process the program starts: no
//! write but into the scratch directory, [`Spec::writable`] and the null devices, no network
//! but the loopback and Unix sockets in those directories (not the DNS resolver's, which a
//! network namespace has none of on Linux, nor any other daemon's), and no Mach service but
//! the few that only answer or log ([`MACH_SERVICES`]): `cfprefsd`, launchd and the rest
//! would write for the program outside. The program's process is a session of its own, with
//! no controlling terminal to push input into. Seatbelt can't make a write private or tell what was read,
//! so a library every process of the run loads (`DYLD_INSERT_LIBRARIES`,
//! `macos_interpose.c`) does that from inside the process: it replaces the C library's calls
//! that name a path, run a program or reach the network, keeps the writes under the overlays
//! in the scratch directory (a changed file is cloned there first, which on APFS copies no
//! data), answers `/tmp`, `/var/tmp` and the user's temporary directory from empty ones of the
//! run's own, and appends what it saw to a file of the run's, from which the record
//! ([`Recorder`]) is made when the run ends.
//!
//! macOS drops `DYLD_*` for the programs it protects (`/bin/sh`, everything in `/usr/bin`) and
//! for hardened ones; such a program still runs under Seatbelt, so it can't write into an
//! overlay or anywhere else outside the run, but what it reads is not seen. Each run of a
//! program is given a nonce the library says back when it loads, and a program that never said
//! it is an `X` line in the record: the record is incomplete.
//!
//! A view ([`Spec::view`]) is not made on macOS (decided, 2026-10-27): a spec with one is run
//! as though it had none, its binds unread, and the program sees the machine as Seatbelt
//! leaves it. Linux is where a build sees its declared closure alone.
//!
//! For `blaze sandbox --v2` ([`super::v2`]) the library appends each line to the live record
//! itself, before the call it notes returns (Seatbelt lets the run write that one file), and
//! sees what the paths don't show: the variables read by name (`getenv`; `_NSGetEnviron` is
//! the whole environment), the realtime clock, randomness (`getentropy`, `arc4random`,
//! `CCRandomGenerateBytes`, the random devices), the directories listed (`getdirentries`,
//! under `readdir`, and `getattrlistbulk`) and, with [`Also::network`], what the network
//! answers (`T network`). A forked process keeps its thread's scope; a program run is told the
//! scope of the thread that ran it.
//!
//! As the program ends, what it left running in its session's process group is killed, as a
//! PID namespace's processes end with its first on Linux, before the record is read; one still
//! there a moment later is an `X` line. A process that left the group (`setsid`, `setpgid`,
//! a spawn into a group of its own) is not killed, there being no PID namespace to tell it by;
//! Seatbelt still confines it, and the library says it left, so one still running as the run
//! ends is an `X` line too.
//!
//! Every system call is behind a safe function of `raw` but `fork`, sound only in a process of
//! one thread: [`run`], which forks, allows unsafe code; the rest of the file denies it.

#![deny(unsafe_code)]

use std::{
    ffi::{CString, OsString},
    fmt::Write as _,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use blaze_hash::HashSet;
use blaze_usercode::trace::Seen;

use super::{Also, Recorder, Spec, tidy};

/// The variables the sandbox gives the program's processes for the library, which takes them
/// out of what the program sees as it loads.
const OURS: [&str; 4] =
    ["DYLD_INSERT_LIBRARIES", "__BLAZE_SANDBOX", "__BLAZE_SANDBOX_EXEC", "__BLAZE_SANDBOX_SCOPE"];

/// The library every process of a run loads, built from `macos_interpose.c` by `build.rs`.
const LIBRARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blaze_sandbox.dylib"));

#[allow(unsafe_code, reason = "the macOS system calls, each behind a safe function")]
mod raw;

/// The Mach services a program of the sandbox may look up; every other lookup is refused. A
/// service runs outside the sandbox, so one that writes for its callers (`cfprefsd`'s
/// preferences, launchd's jobs, LaunchServices' and the background task manager's login items,
/// the keychain) would be a way out: these are the ones that only answer, or only log. Found by
/// running the C toolchain (`cc`, `ld`), `xcrun metal` and `metallib`, a Metal program that
/// compiles a library at run time and an FSEvents stream under a profile that refused every
/// lookup, and adding what each needed (Nix's sandbox on macOS allows the first and the
/// `trustd` ones).
const MACH_SERVICES: &[&str] = &[
    // getpwuid, getgrgid and group membership (Nix: "Allow getpwuid").
    "(global-name \"com.apple.system.opendirectoryd.libinfo\")",
    "(global-name \"com.apple.system.opendirectoryd.membership\")",
    // libSystem's notifications (time zone and network changes).
    "(global-name \"com.apple.system.notification_center\")",
    // The unified log: `os_log`, `syslog`, what a crash or a failed assertion reports.
    "(global-name \"com.apple.logd\")",
    "(global-name \"com.apple.diagnosticd\")",
    "(global-name \"com.apple.system.logger\")",
    // Watching files (the `notify` crate on macOS).
    "(global-name \"com.apple.FSEvents\")",
    // Checking a certificate against the system's roots (a TLS test against a server on
    // loopback); Nix allows these for the derivations that reach the network.
    "(global-name \"com.apple.trustd\")",
    "(global-name \"com.apple.trustd.agent\")",
    // Metal's compiler, which a program that makes a library from source at run time reaches.
    "(xpc-service-name \"com.apple.MTLCompilerService\")",
];

/// The Mach service of the system's resolver, which a program given the network looks names
/// up through.
const RESOLVER_SERVICE: &str = "(global-name \"com.apple.dnssd.service\")";

/// The profile: everything allowed but writing outside the run, the network beyond the
/// loopback (all of it with `network`, and the resolver's socket), a Unix socket outside the
/// run's own and writable directories, and every Mach service but [`MACH_SERVICES`]. Paths
/// come in as parameters (`WRITE0`, `WRITE1`, ..., the live record's `LIVE0`, ..., and
/// `SOCKET0`, ..., each Unix socket the read set names, which it may connect to, and
/// `SERVICE0`, ..., each Mach name it may look up beyond [`MACH_SERVICES`]), which need
/// no quoting. No terminal is written by name: what the program prints goes to the
/// descriptors it was given, which no rule checks again.
fn profile(writes: usize, lives: usize, sockets: usize, services: usize, network: bool) -> String {
    let mut text = String::with_capacity(4096 + 256 * (writes + lives + sockets));
    text.push_str("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*");
    for at in 0..writes {
        _ = write!(text, " (subpath (param \"WRITE{at}\"))");
    }
    for at in 0..lives {
        _ = write!(text, " (literal (param \"LIVE{at}\"))");
    }
    text.push('\n');
    text.push_str(
        "  (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/random\")
  (literal \"/dev/urandom\") (literal \"/dev/dtracehelper\") (literal \"/dev/stdout\")
  (literal \"/dev/stderr\") (regex #\"^/dev/fd/[0-9]+$\"))
(deny network*)
",
    );
    text.push_str(match network {
        false => {
            "(allow network-bind (local ip \"localhost:*\"))
(allow network-inbound (local ip \"localhost:*\"))
(allow network-outbound (remote ip \"localhost:*\"))
"
        }
        true => {
            "(allow network-bind (local ip \"*:*\"))
(allow network-inbound (local ip \"*:*\"))
(allow network-outbound (remote ip \"*:*\"))
(allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))
"
        }
    });
    // A socket's form takes one path: a rule each (two `subpath`s in one keep the first).
    for at in 0..writes {
        let dir = Writable(at);
        _ = writeln!(text, "(allow network-bind network-inbound (local unix-socket {dir}))");
        _ = writeln!(text, "(allow network-outbound (remote unix-socket {dir}))");
    }
    for at in 0..sockets {
        _ = writeln!(text, "(allow network-outbound (remote unix-socket (path-literal (param \"SOCKET{at}\"))))");
    }
    text.push_str("(deny mach-lookup)\n(allow mach-lookup");
    for service in MACH_SERVICES.iter().chain(network.then_some(&RESOLVER_SERVICE)) {
        text.push_str("\n  ");
        text.push_str(service);
    }
    for at in 0..services {
        _ = write!(text, "\n  (global-name (param \"SERVICE{at}\"))");
    }
    text.push_str(")\n");
    text
}

/// The writable directory of the profile's parameter `WRITE<n>`, as a rule's path.
struct Writable(usize);

impl std::fmt::Display for Writable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(subpath (param \"WRITE{}\"))", self.0)
    }
}

/// The program's environment from `given` as it is, without `TMPDIR` (the run's own is given)
/// and the library's variables ([`OURS`]). A name or a value with a nul in it can't be in one.
fn program_env(given: &[(OsString, OsString)]) -> Result<Vec<CString>, String> {
    let mut env = Vec::with_capacity(given.len() + OURS.len() + 1);
    for (name, value) in given {
        if name == "TMPDIR" || OURS.iter().any(|it| name == it) {
            continue;
        }
        // Once per run, the program's environment for the kernel.
        let mut entry = Vec::with_capacity(name.len() + 1 + value.len() + 1);
        entry.extend_from_slice(name.as_bytes());
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        env.push(CString::new(entry).map_err(|_| {
            // alloc: the error, once, and no sandbox is made.
            format!("the variable {:?} has a nul in it", name.to_string_lossy())
        })?);
    }
    Ok(env)
}

fn os(what: &str) -> String {
    // alloc: the error, once, and the sandbox or the run stops.
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn c(bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap_or_default()
}

/// The ways a directory is spelled: as given, and as the file system resolves it (`/tmp` is
/// `/private/tmp`), which is what a program that resolved it names.
fn spellings(dir: &Path) -> Vec<String> {
    // Once per directory of the run, as it is set up.
    let mut out = Vec::with_capacity(4);
    out.push(tidy(dir));
    if let Ok(real) = std::fs::canonicalize(dir) {
        out.push(tidy(&real));
    }
    for at in 0..out.len() {
        if let Some(rest) = out[at].strip_prefix("/private").filter(|rest| rest.starts_with('/')) {
            // alloc: as above.
            let rest = rest.to_owned();
            out.push(rest);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The directory in `parent` that stands in for `dir` (an overlay's upper, a temporary
/// directory's empty one), made: named as the file system names `dir`. A directory's own name
/// is what `getattrlist` says of it, from which `realpath` builds the path, so one named
/// otherwise (`up/0`) turned `/w/flags/target` into `/w/0/target`, which is not there.
fn stand_in(parent: &Path, dir: &Path) -> Result<PathBuf, String> {
    // alloc: once per directory stood in for, as the run is set up.
    let real = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    // alloc: as above.
    let made = real.file_name().map_or_else(|| parent.to_owned(), |name| parent.join(name));
    // alloc: the error, once, and no sandbox is made.
    std::fs::create_dir_all(&made).map_err(|error| format!("{}: {error}", made.display()))?;
    Ok(made)
}

/// The user's temporary directory, as macOS gives it to each user (`/var/folders/...`).
fn user_temp() -> Option<PathBuf> {
    let mut buffer = [0u8; 1024];
    let len = raw::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, &mut buffer);
    if len == 0 || len > buffer.len() {
        return None;
    }
    let text = &buffer[..len - 1];
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(text)))
}

/// Where the library is, written there from this executable the first time: a path named by
/// its contents in the user's cache directory, so each build of blaze has its own and an
/// installed one needs nothing besides itself.
fn library() -> Result<PathBuf, String> {
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let caches = caches
        .or_else(|| std::env::home_dir().map(|it| it.join(".cache")))
        .ok_or("no XDG_CACHE_HOME or home directory to keep the sandbox's library in")?;
    let dir = caches.join("blaze/sandbox").join(crate::content::hex(&blaze_ids::b3::hash(LIBRARY)[..12]));
    let file = dir.join("libblaze_sandbox.dylib");
    // The name is that of the contents it should have; what is there is still compared, so a
    // file left half written, or changed since, is written again.
    if std::fs::read(&file).is_ok_and(|it| it == LIBRARY) {
        return Ok(file);
    }
    // alloc: the error, once per run, and no sandbox is made.
    std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
    // alloc: once per run, the first time the library is written.
    let writing = dir.join(format!("writing-{}", std::process::id()));
    std::fs::write(&writing, LIBRARY)
        .and_then(|()| std::fs::rename(&writing, &file))
        // alloc: the error, once per run, and no sandbox is made.
        .map_err(|it| format!("{}: {it}", file.display()))?;
    Ok(file)
}

/// The run's own directory: the overlays' upper directories, the empty temporary
/// directories, the listings, the shared state and the raw record.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(overlays: usize) -> Result<Scratch, String> {
        let base = std::env::temp_dir();
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| it.subsec_nanos());
        // alloc: once per run, its directory's name.
        let dir = base.join(format!("blaze-sandbox-{}-{nanos:x}", std::process::id()));
        let scratch = Scratch { dir };
        let made = |it: PathBuf| {
            // alloc: the error, once, and no sandbox is made.
            std::fs::create_dir_all(&it).map_err(|error| format!("{}: {error}", it.display()))
        };
        for it in ["list", "tmp/tmp", "tmp/var", "tmp/user"] {
            made(scratch.dir.join(it))?;
        }
        for at in 0..overlays {
            // alloc: once per overlay, its upper directory's name.
            made(scratch.dir.join(format!("up/{at}")))?;
        }
        let state = scratch.dir.join("state");
        std::fs::write(&state, [0u8; 4096])
            // alloc: the error, once, and no sandbox is made.
            .map_err(|error| format!("{}: {error}", state.display()))?;
        Ok(scratch)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::fs::remove_dir_all(&self.dir).is_ok() {
            return;
        }
        // The program may have left a directory it could not be removed from.
        fn writable(dir: &Path) {
            use std::os::unix::fs::PermissionsExt as _;
            _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if entry.file_type().is_ok_and(|it| it.is_dir()) {
                    writable(&entry.path());
                }
            }
        }
        writable(&self.dir);
        _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Runs `program` sandboxed as `spec` and `also` say, or with `None` makes the sandbox and runs
/// nothing in it. What it ends with: its status, or 128 and the signal that ended it.
#[allow(unsafe_code, reason = "fork")]
pub fn run(spec: &Spec, program: Option<&[OsString]>, also: Also<'_>) -> Result<i32, String> {
    let library = library()?;
    let scratch = Scratch::new(spec.overlays.len())?;
    let dir = scratch.dir.to_string_lossy().into_owned();
    let recording = program.is_some() && (spec.record.is_some() || also.live.is_some());
    let live = also.live.filter(|_| recording);

    // What the library is told.
    let mut config = String::with_capacity(4096);
    for (at, overlay) in spec.overlays.iter().enumerate() {
        std::fs::create_dir_all(overlay)
            // alloc: the error, once, and no sandbox is made.
            .map_err(|error| format!("{}: {error}", overlay.display()))?;
        // alloc: once per overlay, its upper directory's name.
        let upper = stand_in(&scratch.dir.join(format!("up/{at}")), overlay)?;
        for it in spellings(overlay) {
            _ = writeln!(config, "O\t{at}\t{it}\t{}", upper.display());
        }
    }
    let user = user_temp();
    let env_temp = spec.env.iter().find(|(name, _)| name == "TMPDIR");
    let env_temp = env_temp.map(|(_, it)| PathBuf::from(it));
    let users = scratch.dir.join("tmp/user");
    let user_in = match user.as_ref().or(env_temp.as_ref()) {
        Some(it) => stand_in(&users, it)?,
        None => users,
    };
    let mut temps = Vec::with_capacity(4);
    for (temp, to) in [("/private/tmp", "tmp"), ("/private/var/tmp", "var")] {
        let temp = PathBuf::from(temp);
        let empty = stand_in(&scratch.dir.join("tmp").join(to), &temp)?;
        temps.push((temp, empty));
    }
    // Both of the user's temporary directories are answered from one empty one.
    // clone: once per run, as above.
    temps.extend(user.map(|it| (it, user_in.clone())));
    // clone: as above.
    temps.extend(env_temp.map(|it| (it, user_in.clone())));
    for (temp, to) in &temps {
        for it in spellings(temp) {
            _ = writeln!(config, "B\t{it}\t{}", to.display());
        }
    }
    // A directory passed through or overlaid below a temporary one is reached through the
    // directories above it, which are answered from the empty one: they are made there, as
    // Linux makes the points it mounts it on. Without them `realpath` (Rust's `canonicalize`),
    // which looks at each directory of a path in turn, says the path is not there: a
    // proc-macro server canonicalizes the library it loads, and every macro of a workspace
    // below `/tmp` failed; a cargo a test runs canonicalizes the workspace once rustc has
    // compiled a crate of it, and failed "could not parse/generate dep info".
    for passed in spec.writable.iter().chain(&spec.readable).chain(&spec.overlays) {
        for (temp, to) in &temps {
            let below = spellings(passed).into_iter().find_map(|passed| {
                spellings(temp).into_iter().find_map(|temp| {
                    Path::new(&passed).strip_prefix(&temp).ok().map(Path::to_owned)
                })
            });
            let Some(below) = below.filter(|it| !it.as_os_str().is_empty()) else { continue };
            let made = to.join(below);
            std::fs::create_dir_all(&made)
                // alloc: the error, once, and no sandbox is made.
                .map_err(|error| format!("{}: {error}", made.display()))?;
        }
    }
    let mut writes = Vec::with_capacity(1 + 4 * spec.writable.len());
    // clone: once per run, the run's own directory as the profile's first parameter.
    writes.push(dir.clone());
    for writable in &spec.writable {
        std::fs::create_dir_all(writable)
            // alloc: the error, once, and no sandbox is made.
            .map_err(|error| format!("{}: {error}", writable.display()))?;
        for it in spellings(writable) {
            _ = writeln!(config, "W\t{it}");
            writes.push(it);
        }
    }
    // A readable directory is passed through as it is, not answered from an empty one: the
    // library takes it as it takes a writable one, and Seatbelt still refuses its writes.
    for readable in &spec.readable {
        for it in spellings(readable) {
            _ = writeln!(config, "W\t{it}");
        }
    }
    // What is seen as it is and is no input: the devices. The program is named in the record
    // as any path: it is no input of the run's own process alone, which `Recorder` keeps
    // to the unscoped lines.
    for it in spellings(Path::new("/dev")) {
        _ = writeln!(config, "Q\t{it}");
    }
    if recording {
        config.push_str("L\n");
        if spec.record_reads {
            config.push_str("R\n");
        }
    }
    let lives = match live {
        Some(live) => {
            let path = live.path.to_str().filter(|it| !it.contains(['\t', '\n']));
            let path = path.ok_or_else(|| {
                // alloc: the error, once, and no sandbox is made.
                format!("the live record {} is not a path the library can be told", live.path.display())
            })?;
            _ = writeln!(config, "V\t{path}");
            spellings(&live.path)
        }
        None => Vec::with_capacity(0),
    };
    if also.network {
        config.push_str("I\n");
    }
    let config_path = scratch.dir.join("config");
    std::fs::write(&config_path, config)
        // alloc: the error, once, and no sandbox is made.
        .map_err(|error| format!("{}: {error}", config_path.display()))?;

    // Everything the child uses is made before the fork, once per run: the profile, its
    // parameters, the program's command line and environment, for the kernel.
    // A socket the read set names (a test's fork server's) is one it connects to.
    let sockets: Vec<String> = spec
        .readable
        .iter()
        .filter(|it| std::fs::metadata(it).is_ok_and(|it| std::os::unix::fs::FileTypeExt::is_socket(&it.file_type())))
        .flat_map(|it| spellings(it))
        // alloc: once per run, as above.
        .collect();
    for service in also.services {
        if service.is_empty() || service.contains('\0') {
            // alloc: the error, once, and no sandbox is made.
            return Err(format!("the Mach service {service:?} can't be named in a profile"));
        }
    }
    let profile = c(profile(writes.len(), lives.len(), sockets.len(), also.services.len(), also.network).as_bytes());
    let mut params = Vec::with_capacity(2 * (writes.len() + lives.len() + sockets.len() + also.services.len()));
    let mut name = String::with_capacity(16);
    for (kind, values) in [("WRITE", &writes), ("LIVE", &lives), ("SOCKET", &sockets)] {
        for (at, it) in values.iter().enumerate() {
            name.clear();
            _ = write!(name, "{kind}{at}");
            params.extend([c(name.as_bytes()), c(it.as_bytes())]);
        }
    }
    for (at, it) in also.services.iter().enumerate() {
        name.clear();
        _ = write!(name, "SERVICE{at}");
        params.extend([c(name.as_bytes()), c(it.as_bytes())]);
    }
    let param_ptrs = raw::Strings::new(&params);
    let program_argv = program.unwrap_or_default();
    let mut argv: Vec<CString> = Vec::with_capacity(program_argv.len());
    argv.extend(program_argv.iter().map(|it| c(it.as_bytes())));
    let argv_ptrs = raw::Strings::new(&argv);
    // alloc: once per run, as above.
    let nonce = format!("{}.top", std::process::id());
    // The program's environment is the spec's (as it is with `also.env`), its `TMPDIR` the
    // run's empty one.
    let mut env: Vec<CString> = match also.env {
        Some(given) => program_env(given)?,
        None => spec
            .env
            .iter()
            .filter(|(name, _)| name != "TMPDIR" && !OURS.contains(&name.as_str()))
            // alloc: once per run, as above.
            .map(|(name, value)| c(format!("{name}={value}").as_bytes()))
            // alloc: as above.
            .collect(),
    };
    // The program's own `DYLD_INSERT_LIBRARIES` loads after the library, which takes it away
    // again before the program starts.
    let theirs: Option<&std::ffi::OsStr> = match also.env {
        Some(given) => given.iter().find(|(name, _)| name == "DYLD_INSERT_LIBRARIES").map(|it| it.1.as_os_str()),
        None => spec.env.iter().find(|(name, _)| name == "DYLD_INSERT_LIBRARIES").map(|it| std::ffi::OsStr::new(&it.1)),
    };
    let mut insert_var = OsString::from("DYLD_INSERT_LIBRARIES=");
    insert_var.push(library.as_os_str());
    if let Some(theirs) = theirs.filter(|it| !it.is_empty()) {
        insert_var.push(":");
        insert_var.push(theirs);
    }
    env.push(c(insert_var.as_bytes()));
    // alloc: once per run, as above.
    env.push(c(format!("TMPDIR={}/", user_in.display()).as_bytes()));
    // alloc: once per run, as above.
    env.push(c(format!("__BLAZE_SANDBOX={dir}").as_bytes()));
    // alloc: once per run, as above.
    env.push(c(format!("__BLAZE_SANDBOX_EXEC={nonce}").as_bytes()));
    let env_ptrs = raw::Strings::new(&env);
    let cwd = c(spec.cwd.as_os_str().as_bytes());
    let raw = scratch.dir.join("record");
    if recording {
        let program = program.unwrap_or_default().first().map(|it| it.to_string_lossy());
        // alloc: once per run, the record's first line.
        let line = format!("P\t{nonce}\t\t{}\n", program.unwrap_or_default());
        // alloc: the error, once, and no sandbox is made.
        std::fs::write(&raw, line).map_err(|error| format!("{}: {error}", raw.display()))?;
    }

    // Before the fork, so that no signal ends this process before it passes them on.
    super::pass_signals_to(0);
    // SAFETY: the process is one thread: `super::command` runs before anything else in it (or
    // a test's `super::alone` before `main`), and nothing here starts a thread. No lock is
    // held by another thread across the fork, so the child need not keep to async-signal-safe
    // calls: it applies the profile, may allocate and print why it failed, sleeps between
    // tries, and runs the program or exits.
    let child = unsafe { raw::fork() };
    if child < 0 {
        return Err(os("fork"));
    }
    if child == 0 {
        // A session of its own, with no controlling terminal: nothing of the sandbox can push
        // input into the caller's (`TIOCSTI` is refused but to the terminal's own session).
        raw::setsid();
        if let Err(why) = raw::sandbox_init(&profile, &param_ptrs) {
            eprintln!("blaze: the sandbox could not be made: Seatbelt refused the profile: {why}");
            raw::exit(super::COULD_NOT);
        }
        if program.is_none() {
            raw::exit(0);
        }
        if raw::chdir(&cwd) < 0 {
            // alloc: the error, once, and the program does not run.
            let error = os(&format!("the working directory {}", spec.cwd.display()));
            eprintln!("blaze: the sandbox could not be made: {error}");
            raw::exit(super::COULD_NOT);
        }
        // The program is often the file blaze just wrote: an exec that meets a writer's
        // descriptor not yet closed (`ETXTBSY`) is made again on blaze-core's schedule.
        let (mut tried, mut waited) = (0, 0);
        let said = loop {
            raw::execve(&argv_ptrs, &env_ptrs);
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ETXTBSY) {
                // alloc: the error, once, and the program's process ends.
                break error.to_string();
            }
            let Some(wait) = blaze_core::busy::next(tried, waited) else {
                // alloc: as above.
                break format!(
                    "still open for writing after {} ms of tries: {error}",
                    blaze_core::busy::MOST_NS / 1_000_000
                );
            };
            blaze_core::busy::wait(wait);
            tried += 1;
            waited += wait;
        };
        eprintln!("blaze: could not run {}: {said}", argv[0].to_string_lossy());
        raw::exit(127);
    }
    super::pass_signals_to(child);
    // Waited for, not reaped yet, so that its group's id is no other's when what it left
    // running in its group is ended, as a PID namespace's processes end with its first on
    // Linux: what they did later would be in no record.
    while raw::wait_unreaped(child) < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitid"));
        }
    }
    // `child` has ended but is not reaped (`WNOWAIT` above), so its group's id is no other
    // process's; once reaped it could be, and this would kill that group.
    raw::kill(-child, libc::SIGKILL);
    let mut status = 0;
    while raw::wait(child, &mut status, 0) < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(os("waitpid"));
        }
    }
    // Its number may be another's now.
    super::pass_signals_to(0);
    let left = recording && !group_ended(child);
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    };
    if recording {
        // The paths the library saw are as the program spelled them, so the overlays and what
        // is no input are known by each of their spellings. Once per run, as it ends.
        let spelled = |dirs: &[PathBuf]| -> Vec<PathBuf> {
            // alloc: as above.
            dirs.iter().flat_map(|it| spellings(it)).map(PathBuf::from).collect()
        };
        // clone: as above, the spec the recorder is made from.
        let mut known = spec.clone();
        known.overlays = spelled(&spec.overlays);
        known.not_inputs = spelled(&spec.not_inputs);
        let mut own = Vec::with_capacity(1 + 4 * temps.len());
        own.extend(["/dev"].into_iter().map(str::to_owned));
        own.extend(temps.iter().flat_map(|(it, _)| spellings(it)));
        let mut own_names: Vec<&str> = Vec::with_capacity(own.len());
        own_names.extend(own.iter().map(String::as_str));
        let own = own_names;
        let mut recorder = Recorder::new(&known, &own);
        if let Some(live) = live
            && let Err(why) = recorder.live(&live.path)
        {
            live.failed(why);
        }
        let lost = match std::fs::read(&raw) {
            // alloc: once, as the run ends: the record read as text, where it is not UTF-8.
            Ok(bytes) => merge(&String::from_utf8_lossy(&bytes), &mut recorder),
            Err(error) => {
                // alloc: the error, once, as the run ends.
                let why = format!("the sandbox's record {}: {error}", raw.display());
                recorder.reached(Seen::Incomplete, "", &why);
                None
            }
        };
        if left {
            let why = "processes of the run still ran after it ended, so what they read is not known";
            recorder.reached(Seen::Incomplete, "", why);
        }
        if let Some(file) = &spec.record {
            recorder.write(file);
        }
        if let Some(live) = live {
            if let Some(why) = lost {
                // alloc: the error, once, as the run ends.
                live.failed(format!("the sandbox's library could not add a line to it: {why}"));
            }
            if let Some(why) = recorder.live_failed() {
                live.failed(why);
            }
        }
    }
    drop(scratch);
    Ok(code)
}

/// Whether the process group `group` has ended, given a moment for what was killed in it to
/// go: a process that left it (`setsid`) is not waited for.
fn group_ended(group: libc::pid_t) -> bool {
    for _ in 0..2000 {
        // Signal 0 sends nothing: it only asks whether the group is there.
        if raw::kill(-group, 0) < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        #[allow(clippy::disallowed_methods, reason = "the group's timer: macOS has no event for a process group's end, only each known pid's")]
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    false
}

/// Whether a process `pid` is there: one that is another user's is too. Its number may be
/// another's now, which only makes the record say it is not whole when it is.
fn still_runs(pid: libc::pid_t) -> bool {
    // Signal 0 sends nothing: it only asks whether the process is there.
    raw::kill(pid, 0) == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Gives `recorder` what the processes of a run appended to `raw` (`macos_interpose.c` says
/// what each line is), once the run has ended: the record's lines, there when there is no
/// live record (the library appends them to it itself otherwise), an `X` line for each program
/// that ran without the library, for each process that left the run's process group and still
/// runs, and for a line that is not one. Why a line could not be added to the live record,
/// when the library said one could not.
fn merge(raw: &str, recorder: &mut Recorder) -> Option<String> {
    // Once per run, as it ends: the programs it ran, by reference.
    let mut ran = Vec::with_capacity(64);
    let mut loaded = HashSet::default();
    let mut lost = None;
    for piece in raw.split_inclusive('\n') {
        let Some(line) = piece.strip_suffix('\n') else {
            // Every process has ended: a line with no line break was written short.
            let why = "the sandbox's record ends in a line cut short";
            recorder.reached(Seen::Incomplete, "", why);
            break;
        };
        let mut fields = line.splitn(4, '\t');
        let seen = match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some("P"), Some(nonce), Some(test), Some(path)) => {
                ran.push((nonce, test, path));
                continue;
            }
            (Some("H" | "F"), Some(nonce), None, None) => {
                loaded.insert(nonce);
                continue;
            }
            (Some("S"), Some(pid), None, None) if pid.parse::<libc::pid_t>().is_ok_and(|it| it > 0) => {
                if still_runs(pid.parse().unwrap_or_default()) {
                    // alloc: once per process left running, and the record is not whole.
                    let why = format!(
                        "process {pid} of the run left its process group and still ran after the \
                         run ended, so what it read is not known"
                    );
                    recorder.reached(Seen::Incomplete, "", &why);
                }
                continue;
            }
            (Some("Z"), Some(errno), None, None) => {
                let why = match errno.parse::<i32>() {
                    // alloc: the error, and the live record is not whole.
                    Ok(errno) if errno > 0 => std::io::Error::from_raw_os_error(errno).to_string(),
                    // alloc: as above.
                    _ => "it was written short".to_owned(),
                };
                lost.get_or_insert(why);
                continue;
            }
            (Some(letter), Some(test), Some(what), None) => Seen::of(letter).map(|it| (it, test, what)),
            _ => None,
        };
        match seen {
            Some((seen, test, what)) => recorder.reached(seen, test, what),
            None => {
                // alloc: a line that is not one, and the record is not whole.
                let shown: String = line.chars().take(200).collect();
                // alloc: as above.
                let why = format!("the sandbox's record has a line that is not one: {shown:?}");
                recorder.reached(Seen::Incomplete, "", &why);
            }
        }
    }
    for (nonce, test, path) in ran {
        if !loaded.contains(nonce) {
            // alloc: once per program run without the library, and the record is not whole.
            let why = format!(
                "{path} ran without the recorder (macOS keeps DYLD_INSERT_LIBRARIES from a \
                 program it protects), so what it read is not known"
            );
            recorder.reached(Seen::Incomplete, test, &why);
        }
    }
    lost
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use blaze_usercode::trace::{self, Seen};

    use super::super::{Recorder, Spec};

    #[test]
    fn merge_keeps_each_line_once_and_names_the_programs_not_seen() {
        let raw = "P\t1.top\t\t/w/target/debug/deps/t-1\n\
                   H\t1.top\n\
                   R\ta::b\t/w/data.txt\n\
                   R\ta::b\t/w/data.txt\n\
                   R\t\t/dev/null\n\
                   R\t\t/w/target/debug/deps/t-1\n\
                   D\ta::b\t/w/src\n\
                   E\ta::b\tHOME\n\
                   T\ta::b\tclock\n\
                   W\ta::b\t/Users/u/.config/x\n\
                   N\ta::c\t1.1.1.1:443\n\
                   P\t5.ab.0\ta::c\t/bin/sh\n\
                   P\t5.ab.1\ta::c\t/w/target/debug/helper\n\
                   H\t5.ab.1\n\
                   P\t5.ab.2\ta::c\t/missing\n\
                   F\t5.ab.2\n\
                   X\ta::d\ta path or a name with a tab or a line break in it\n\
                   Q\tnot\ta line\n\
                   R\ta::b\t/w/last";
        let spec = Spec {
            overlays: vec!["/w".into()],
            not_inputs: vec!["/w/target/debug/deps/t-1".into()],
            record_reads: true,
            ..Spec::default()
        };
        let mut recorder = Recorder::new(&spec, &["/dev"]);
        assert_eq!(super::merge(raw, &mut recorder), None);
        let kept = |seen: Seen, what: &str| recorder.lines().any(|it| it.seen == seen && it.what == what);
        assert!(kept(Seen::Env, "HOME") && kept(Seen::Untraceable, "clock"));
        let dir = std::env::temp_dir().join(format!("blaze-macos-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("record");
        recorder.write(&file);
        let text = std::fs::read_to_string(&file).unwrap();
        _ = std::fs::remove_dir_all(&dir);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8, "{text}");
        assert_eq!(lines[0], "R\ta::b\t/w/data.txt");
        assert_eq!(lines[1], "R\ta::b\t/w/src", "a listing is a read in this record");
        assert_eq!(lines[2], "W\ta::b\t/Users/u/.config/x");
        assert_eq!(lines[3], "N\ta::c\t1.1.1.1:443");
        assert_eq!(lines[4], "X\t\tthe sandbox's record ends in a line cut short");
        assert!(lines[5].starts_with("X\t\tthe sandbox's record has a line that is not one"), "{text}");
        assert!(lines[6].starts_with("X\ta::c\t/bin/sh ran without the recorder"), "{text}");
        assert_eq!(lines[7], "X\ta::d\ta path or a name with a tab or a line break in it");
    }

    #[test]
    fn a_line_the_library_could_not_add_is_said() {
        let mut recorder = Recorder::new(&Spec::default(), &[]);
        let lost = super::merge("H\t1.top\nZ\t28\nZ\t5\n", &mut recorder).unwrap();
        assert_eq!(lost, std::io::Error::from_raw_os_error(28).to_string(), "the first is kept");
        assert_eq!(super::merge("Z\t0\n", &mut recorder).as_deref(), Some("it was written short"));
        assert_eq!(recorder.lines().count(), 0);
    }

    #[test]
    fn a_process_that_left_the_group_and_still_runs_makes_the_record_not_whole() {
        let mut gone = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let gone_pid = gone.id();
        gone.wait().unwrap();
        let mut recorder = Recorder::new(&Spec::default(), &[]);
        let raw = format!("S\t{gone_pid}\nS\t{}\n", std::process::id());
        assert_eq!(super::merge(&raw, &mut recorder), None);
        let lines: Vec<_> = recorder.lines().collect();
        let here = format!("process {} of the run left its process group", std::process::id());
        assert!(lines.iter().any(|it| it.seen == Seen::Incomplete && it.what.starts_with(&here)), "{lines:?}");
        let gone = format!("process {gone_pid} ");
        assert!(!lines.iter().any(|it| it.what.starts_with(&gone)), "{lines:?}");
    }

    #[test]
    fn the_profile_lets_the_live_record_be_written_and_the_network_through_when_given() {
        let closed = super::profile(2, 1, 1, 0, false);
        assert!(closed.contains("(subpath (param \"WRITE1\"))"), "{closed}");
        assert!(closed.contains("(literal (param \"LIVE0\"))"), "{closed}");
        assert!(closed.contains("(remote unix-socket (path-literal (param \"SOCKET0\")))"), "{closed}");
        assert!(closed.contains("(remote ip \"localhost:*\")"), "{closed}");
        assert!(!closed.contains("\"*:*\"") && !closed.contains("mDNSResponder"), "{closed}");
        assert!(!closed.contains("com.apple.dnssd.service"), "{closed}");
        let open = super::profile(1, 0, 0, 0, true);
        assert!(!open.contains("LIVE0") && !open.contains("SOCKET0"), "{open}");
        assert!(open.contains("(allow network-outbound (remote ip \"*:*\"))"), "{open}");
        assert!(open.contains("/private/var/run/mDNSResponder"), "{open}");
        assert!(open.contains(super::RESOLVER_SERVICE), "{open}");
        assert!(open.contains("(deny mach-lookup)"), "the other services stay refused");
        assert!(!open.contains("SERVICE0"), "{open}");
    }

    #[test]
    fn each_mach_name_given_is_one_more_lookup_and_no_other_is_allowed() {
        let profile = super::profile(0, 0, 0, 2, false);
        let denied = profile.find("(deny mach-lookup)").expect("other lookups are refused");
        let allowed = profile[denied..].find("(allow mach-lookup").expect("the profile's own are allowed") + denied;
        // The allowed list is the profile's last form.
        let list = &profile[allowed..];
        assert!(list.ends_with(")\n"), "{profile}");
        for at in 0..2 {
            assert!(list.contains(&format!("(global-name (param \"SERVICE{at}\"))")), "{profile}");
        }
        assert!(!profile.contains("SERVICE2"), "{profile}");
    }

    #[test]
    fn the_program_is_given_its_environment_as_it_is_without_the_sandboxs() {
        use std::os::unix::ffi::OsStringExt as _;
        let given: Vec<(OsString, OsString)> = vec![
            ("A".into(), OsString::from_vec(b"\xff".to_vec())),
            ("TMPDIR".into(), "/x".into()),
            ("DYLD_INSERT_LIBRARIES".into(), "/l.dylib".into()),
            ("__BLAZE_SANDBOX_SCOPE".into(), "t".into()),
            ("B".into(), "2".into()),
        ];
        let env = super::program_env(&given).unwrap();
        let env: Vec<&[u8]> = env.iter().map(|it| it.as_bytes()).collect();
        assert_eq!(env, [&b"A=\xff"[..], b"B=2"]);
        let nul = vec![("A".into(), OsString::from_vec(b"x\0y".to_vec()))];
        assert!(super::program_env(&nul).unwrap_err().contains("nul"));
    }

    /// What the end-to-end test runs in the sandbox: this executable, which macOS does not
    /// protect, so the library is in it. Does nothing unless that test runs it.
    #[test]
    fn helper_reads_a_variable_lists_and_reads_the_clock() {
        let Some(dir) = std::env::var_os("BLAZE_SANDBOX_TEST_HELPER") else { return };
        let value = std::env::var("BLAZE_SANDBOX_TEST_VAR").unwrap();
        let names = std::fs::read_dir(&dir).unwrap().count();
        let now = std::time::SystemTime::now();
        assert!(now > std::time::UNIX_EPOCH);
        std::fs::write(std::path::Path::new(&dir).join("seen"), format!("{value} {names}")).unwrap();
    }

    /// A run of `blaze sandbox --v2` with the library in its program: its variables read by
    /// name, its listing and its clock are in the live record, it is whole, and the report has
    /// them. Skipped where no sandbox can be made, unless `BLAZE_VIEW_TESTS=require`.
    #[test]
    fn a_traced_run_sees_variables_listings_and_the_clock() {
        if let Err(why) = crate::sandbox::alone::probe(crate::sandbox::Probe::V2) {
            let required = std::env::var("BLAZE_VIEW_TESTS").is_ok_and(|it| it == "require");
            assert!(!required, "{why}");
            eprintln!("skipped: {why}");
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let dir = std::env::temp_dir().join(format!("blaze-macos-v2-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a"), "").unwrap();
        let spec = blaze_usercode::confine::Spec {
            program: exe.clone(),
            args: ["sandbox::macos::tests::helper_reads_a_variable_lists_and_reads_the_clock", "--exact", "--test-threads=1"]
                .map(OsString::from)
                .to_vec(),
            env: vec![
                ("BLAZE_SANDBOX_TEST_HELPER".into(), out.clone().into()),
                ("BLAZE_SANDBOX_TEST_VAR".into(), "v".into()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ],
            dir: out.clone(),
            read: vec![dir.clone()],
            write: vec![out.clone()],
            network: false,
            trace: true,
            services: Vec::new(),
            live: dir.join("live"),
            report: dir.join("report"),
            not_inputs: vec![exe],
        };
        assert_eq!(crate::sandbox::alone::launch(&spec), 0);
        assert_eq!(std::fs::read_to_string(out.join("seen")).unwrap(), "v 1");
        let text = std::fs::read_to_string(&spec.live).unwrap();
        let (lines, used) = trace::parse(&text).unwrap();
        assert_eq!(used, text.len(), "{text}");
        assert_eq!(lines[0], trace::Line::new(Seen::Env, "", trace::ALL_ENV));
        assert!(!lines.iter().any(|it| it.seen == Seen::Incomplete), "{text}");
        let has = |seen: Seen, what: &str| lines.iter().any(|it| it.seen == seen && it.what == what);
        assert!(has(Seen::Env, "BLAZE_SANDBOX_TEST_VAR"), "{text}");
        assert!(has(Seen::Untraceable, "clock"), "{text}");
        let listed = std::fs::canonicalize(&out).unwrap();
        let listed = lines.iter().any(|it| {
            it.seen == Seen::Listed && std::fs::canonicalize(&it.what).is_ok_and(|it| it == listed)
        });
        assert!(listed, "{text}");
        let report = trace::decode(&std::fs::read(&spec.report).unwrap()).unwrap();
        let trace::Report::Ran(Some(traced)) = report else { panic!("{report:?}") };
        assert!(traced.env.iter().any(|it| it.0 == "BLAZE_SANDBOX_TEST_VAR" && it.1.as_deref() == Some("v".as_ref())));
        assert!(traced.untraceable.contains(&blaze_core::Untraceable::Clock));
        _ = std::fs::remove_dir_all(&dir);
    }
}
