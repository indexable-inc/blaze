//! The units that socket-activate the daemon ([`crate::activation`];
//! docs/v2/plan/lanes/frontend/activate.md), made from one [`Unit`]: a launchd agent on
//! macOS, a systemd user socket and service on Linux. The manager listens on the socket the
//! CLI connects to and starts `blaze serve --activated` at the first connection; nothing waits
//! for a start step.
//!
//! `blaze daemon install` runs the [`Step`]s of [`install`], `blaze daemon uninstall` those of
//! [`uninstall`]; the CLI spawns a daemon itself only where [`installed`] says no unit serves
//! its socket.

use std::{
    fmt::{self, Write as _},
    path::{Path, PathBuf},
};

use crate::activation::{Manager, SOCKET};

/// launchd's label of the agent, and its plist's name.
pub const LABEL: &str = "inc.indexable.blaze";
/// The plist's file name: [`LABEL`] and `.plist`.
const PLIST_FILE: &str = "inc.indexable.blaze.plist";
pub const SOCKET_UNIT: &str = "blaze.socket";
pub const SERVICE_UNIT: &str = "blaze.service";
/// The variables of the installing shell the daemon is given, sorted: what it reads to find
/// its state, socket and store, and the `PATH` and `HOME` of what it runs. `BLAZE_SANDBOX` is
/// never one: a sandbox turned off for one shell stays a choice of that shell.
pub const CAPTURED: [&str; 5] = ["BLAZE_CACHE", "BLAZE_HOME", "HOME", "PATH", "XDG_CACHE_HOME"];
/// A file the CLI writes says so in its first line.
const WRITTEN: &str = "Written by `blaze daemon install`; `blaze daemon uninstall` removes it.";
/// How soon launchd starts the daemon again after it exits: its default of 10 s would hold a
/// connection to a daemon that just stopped for that long.
const THROTTLE_SECS: u32 = 1;

/// What the daemon is and where its socket is: the one description every unit is made from.
// clone: once per `blaze daemon install`; the tests make variants of one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unit {
    /// blaze's executable, absolute.
    pub program: PathBuf,
    /// The socket the CLI connects to, absolute.
    pub socket: PathBuf,
    /// The file the daemon's output is appended to, absolute.
    pub log: PathBuf,
    /// The daemon's environment, of [`CAPTURED`]'s names.
    pub env: Vec<(String, String)>,
}

/// Where the units go and whose they are.
// clone: as `Unit`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    pub manager: Manager,
    /// [`unit_dir`].
    pub dir: PathBuf,
    /// The user's ID: launchd's domain is `gui/<uid>`.
    pub uid: u32,
}

/// The directory a user's units are read from: `~/Library/LaunchAgents`, or systemd's
/// `$XDG_CONFIG_HOME/systemd/user` (`~/.config` without it).
pub fn unit_dir(manager: Manager, home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    match manager {
        Manager::Launchd => home.join("Library/LaunchAgents"),
        Manager::Systemd => match xdg_config_home {
            Some(config) if config.is_absolute() => config.join("systemd/user"),
            _ => home.join(".config/systemd/user"),
        },
    }
}

/// The file whose being there says the units are installed: the plist, or the socket unit.
pub fn main_file(place: &Place) -> PathBuf {
    match place.manager {
        Manager::Launchd => place.dir.join(PLIST_FILE),
        Manager::Systemd => place.dir.join(SOCKET_UNIT),
    }
}

/// A file to write.
// clone: as `Step`, which holds it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnitFile {
    pub path: PathBuf,
    pub text: String,
}

/// One step of an install or uninstall, run in order; the first that fails stops it, named.
// clone: once per `blaze daemon install` or `uninstall`; the tests compare steps they kept.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// The directory and its parents, made mode 0700 where missing.
    MakeDir(PathBuf),
    /// Written whole, mode 0600, replacing what is there.
    Write(UnitFile),
    /// The command, which must exit 0.
    Run(Vec<String>),
    /// `argv` undoes what may not have been done: when it fails, `loaded` is run, and the step
    /// fails only when `loaded` exits 0 (there was something, and it was not undone).
    Undo { argv: Vec<String>, loaded: Vec<String> },
    /// The file removed; one that is not there is removed already.
    Remove(PathBuf),
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::MakeDir(path) => write!(f, "making the directory {}", path.display()),
            Step::Write(file) => write!(f, "writing {}", file.path.display()),
            Step::Run(argv) | Step::Undo { argv, .. } => write!(f, "running `{}`", argv.join(" ")),
            Step::Remove(path) => write!(f, "removing {}", path.display()),
        }
    }
}

/// Why no unit can be made from a [`Unit`].
#[derive(PartialEq, Eq, Debug)]
pub struct UnitError {
    pub what: String,
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze daemon install: {}", self.what)
    }
}

impl std::error::Error for UnitError {}

fn refused(what: String) -> UnitError {
    UnitError { what }
}

/// The units' files, made from `unit`: the plist, or the socket unit then the service.
pub fn files(unit: &Unit, place: &Place) -> Result<Vec<UnitFile>, UnitError> {
    let checked = Checked::of(unit, place.manager)?;
    // alloc: error path, a unit refused once per `blaze daemon install`.
    let dir = place.dir.to_str().ok_or_else(|| refused(format!("the units' directory {} is not UTF-8", place.dir.display())))?;
    if !place.dir.is_absolute() {
        // alloc: error path, a unit refused once per `blaze daemon install`.
        return Err(refused(format!("the units' directory {dir} is not an absolute path")));
    }
    plain("the units' directory", dir)?;
    Ok(match place.manager {
        // alloc: the files, once per `blaze daemon install`.
        Manager::Launchd => vec![UnitFile { path: main_file(place), text: plist(&checked) }],
        // alloc: as above.
        Manager::Systemd => vec![
            UnitFile { path: place.dir.join(SOCKET_UNIT), text: socket_unit(&checked) },
            UnitFile { path: place.dir.join(SERVICE_UNIT), text: service_unit(&checked) },
        ],
    })
}

/// Writes the units and has the manager listen on the socket. One installed already (for
/// this socket or another) is replaced: its daemon stops and the next connection starts the
/// new one.
pub fn install(unit: &Unit, place: &Place) -> Result<Vec<Step>, UnitError> {
    let files = files(unit, place)?;
    // alloc: the steps, once per `blaze daemon install` or `uninstall`.
    let mut steps = vec![Step::MakeDir(place.dir.clone())];
    for path in [&unit.socket, &unit.log] {
        if let Some(parent) = path.parent() {
            steps.push(Step::MakeDir(parent.to_path_buf()));
        }
    }
    match place.manager {
        Manager::Launchd => {
            let (domain, service) = launchd_names(place);
            steps.push(Step::Undo {
                argv: words(&["launchctl", "bootout", &service]),
                loaded: words(&["launchctl", "print", &service]),
            });
            steps.extend(files.into_iter().map(Step::Write));
            let plist = main_file(place);
            let plist = plist.to_str().expect("files() checked the units' directory is UTF-8");
            steps.push(Step::Run(words(&["launchctl", "enable", &service])));
            steps.push(Step::Run(words(&["launchctl", "bootstrap", &domain, plist])));
        }
        Manager::Systemd => {
            steps.extend(files.into_iter().map(Step::Write));
            steps.push(Step::Run(words(&["systemctl", "--user", "daemon-reload"])));
            steps.push(Step::Run(words(&["systemctl", "--user", "enable", SOCKET_UNIT])));
            steps.push(Step::Run(words(&["systemctl", "--user", "restart", SOCKET_UNIT])));
        }
    }
    Ok(steps)
}

/// Stops the daemon, has the manager stop listening, and removes the units and the socket
/// `socket` they made ([`socket_of`] the main file).
pub fn uninstall(place: &Place, socket: &Path) -> Vec<Step> {
    // alloc: the steps, once per `blaze daemon install` or `uninstall`.
    let mut steps = Vec::new();
    match place.manager {
        Manager::Launchd => {
            let (_, service) = launchd_names(place);
            steps.push(Step::Undo {
                argv: words(&["launchctl", "bootout", &service]),
                loaded: words(&["launchctl", "print", &service]),
            });
            steps.push(Step::Remove(main_file(place)));
        }
        Manager::Systemd => {
            steps.push(Step::Run(words(&["systemctl", "--user", "disable", "--now", SOCKET_UNIT])));
            steps.push(Step::Run(words(&["systemctl", "--user", "stop", SERVICE_UNIT])));
            steps.push(Step::Remove(place.dir.join(SOCKET_UNIT)));
            steps.push(Step::Remove(place.dir.join(SERVICE_UNIT)));
            steps.push(Step::Run(words(&["systemctl", "--user", "daemon-reload"])));
        }
    }
    steps.push(Step::Remove(socket.to_path_buf()));
    steps
}

/// Runs `steps` in order; the first that fails stops them, with what it said.
pub fn run(steps: &[Step]) -> Result<(), UnitError> {
    // alloc: error path, a step that failed.
    let failed = |step: &Step, what: String| refused(format!("{step}: {what}"));
    for step in steps {
        match step {
            // alloc: error path, as `failed`.
            Step::MakeDir(path) => private_dir(path).map_err(|error| failed(step, error.to_string()))?,
            // alloc: error path, as `failed`.
            Step::Write(file) => private_write(&file.path, &file.text).map_err(|error| failed(step, error.to_string()))?,
            Step::Run(argv) => {
                if let Err(said) = command(argv) {
                    return Err(failed(step, said));
                }
            }
            Step::Undo { argv, loaded } => {
                if let Err(said) = command(argv)
                    && command(loaded).is_ok()
                {
                    // alloc: error path, as `failed`.
                    return Err(failed(step, format!("{said}; and `{}` says it is still loaded", loaded.join(" "))));
                }
            }
            Step::Remove(path) => match std::fs::remove_file(path) {
                // alloc: error path, as `failed`.
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(failed(step, error.to_string())),
                _ => {}
            },
        }
    }
    Ok(())
}

/// `argv` run to its end: what it said when it did not exit 0.
fn command(argv: &[String]) -> Result<(), String> {
    // alloc: error path, a command that failed, once per step.
    let Some((program, args)) = argv.split_first() else { return Err("an empty command".to_owned()) };
    let ran = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        // alloc: error path, as above.
        .map_err(|error| format!("{program} did not start: {error}"))?;
    if ran.status.success() {
        return Ok(());
    }
    // alloc: error path, as above: the rest of `command` runs only when it failed.
    let said = String::from_utf8_lossy(&ran.stderr);
    // alloc: error path, as above.
    let said = if said.trim().is_empty() { String::from_utf8_lossy(&ran.stdout) } else { said };
    // alloc: error path, as above.
    Err(format!("{} ({})", said.trim(), ran.status))
}

#[cfg(unix)]
fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// `text` written whole next to `path`, mode 0600, and renamed over it.
fn private_write(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    // alloc: the temporary's name, once per file an install writes.
    name.push(format!(".{}.tmp", std::process::id()));
    let temporary = path.with_file_name(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&temporary).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    match written.and_then(|()| std::fs::rename(&temporary, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Whether a unit serves the socket the CLI connects to.
#[derive(PartialEq, Eq, Debug)]
pub enum Installed {
    No,
    Yes,
    /// Installed, for this other socket (another `BLAZE_HOME`).
    Elsewhere(PathBuf),
}

/// Whether the units at `place` serve `socket`, read from the main file; one that is there and
/// names no socket is an error naming it.
pub fn installed(place: &Place, socket: &Path) -> Result<Installed, UnitError> {
    let path = main_file(place);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Installed::No),
        // alloc: error path, a main file that cannot be read.
        Err(error) => return Err(refused(format!("reading {}: {error}", path.display()))),
    };
    match socket_of(place.manager, &text) {
        Some(theirs) if theirs == socket => Ok(Installed::Yes),
        Some(theirs) => Ok(Installed::Elsewhere(theirs)),
        // alloc: error path, a main file naming no socket.
        None => Err(refused(format!(
            "{} names no socket; `blaze daemon uninstall` removes it and `blaze daemon install` writes it again",
            path.display()
        ))),
    }
}

/// The socket a main file this module wrote listens on.
pub fn socket_of(manager: Manager, text: &str) -> Option<PathBuf> {
    match manager {
        Manager::Launchd => {
            let after = text.split_once("<key>SockPathName</key>")?.1.trim_start();
            let value = after.strip_prefix("<string>")?.split_once("</string>")?.0;
            Some(PathBuf::from(xml_text(value)?))
        }
        Manager::Systemd => {
            let line = text.lines().find_map(|line| line.strip_prefix("ListenStream="))?;
            Some(PathBuf::from(line.replace("%%", "%")))
        }
    }
}

/// This user's [`Place`], from the environment (`HOME`, `XDG_CONFIG_HOME`) and the process's
/// user.
#[cfg(unix)]
pub fn place_here() -> Result<Place, UnitError> {
    let manager = Manager::here().ok_or_else(|| {
        // alloc: error path, no manager here.
        refused("no service manager blaze can be socket-activated by runs here: systemd on Linux, launchd on macOS".to_owned())
    })?;
    let home = std::env::var_os("HOME")
        .filter(|it| !it.is_empty())
        .map(PathBuf::from)
        // alloc: error path, no HOME.
        .ok_or_else(|| refused("HOME is not set, and the units go under it".to_owned()))?;
    let config = std::env::var_os("XDG_CONFIG_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    Ok(Place { manager, dir: unit_dir(manager, &home, config.as_deref()), uid: crate::activation::uid() })
}

/// The [`Unit`] of `program` serving `socket`, logging to `log`, with this process's
/// [`CAPTURED`] variables, and this user's [`Place`] for it.
#[cfg(unix)]
pub fn here(program: PathBuf, socket: PathBuf, log: PathBuf) -> Result<(Unit, Place), UnitError> {
    let place = place_here()?;
    let env = captured(|name| std::env::var(name).ok());
    Ok((Unit { program, socket, log, env }, place))
}

/// The variables of [`CAPTURED`] that `get` has, as [`Unit::env`] takes them.
pub fn captured(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    CAPTURED
        .iter()
        // alloc: the variables, once per `blaze daemon install`.
        .filter_map(|name| Some(((*name).to_owned(), get(name).filter(|it| !it.is_empty())?)))
        // alloc: as above.
        .collect()
}

/// A [`Unit`] whose paths are absolute UTF-8 text a unit file can hold.
struct Checked<'a> {
    program: &'a str,
    socket: &'a str,
    log: &'a str,
    /// Sorted by name.
    env: Vec<(&'a str, &'a str)>,
}

impl<'a> Checked<'a> {
    fn of(unit: &'a Unit, manager: Manager) -> Result<Checked<'a>, UnitError> {
        let path = |what: &str, path: &'a Path| -> Result<&'a str, UnitError> {
            // alloc: error path, a unit refused once per `blaze daemon install`.
            let text = path.to_str().ok_or_else(|| refused(format!("{what} {} is not UTF-8", path.display())))?;
            if !path.is_absolute() {
                // alloc: error path, a unit refused once per `blaze daemon install`.
                return Err(refused(format!("{what} {text} is not an absolute path")));
            }
            plain(what, text)?;
            if manager == Manager::Systemd
                && what != "blaze's executable"
                && let Some(bad) = text.chars().find(|it| it.is_whitespace() || *it == '\\')
            {
                // alloc: error path, a unit refused once per `blaze daemon install`.
                return Err(refused(format!("{what} {text:?} holds {bad:?}, which a systemd unit cannot name it with")));
            }
            Ok(text)
        };
        let checked = Checked {
            program: path("blaze's executable", &unit.program)?,
            socket: path("the socket", &unit.socket)?,
            log: path("the log", &unit.log)?,
            // alloc: the variables, borrowed, once per `blaze daemon install`.
            env: unit.env.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect(),
        };
        let longest = match manager {
            Manager::Launchd => 103,
            Manager::Systemd => 107,
        };
        if checked.socket.len() > longest {
            // alloc: error path, a unit refused once per `blaze daemon install`.
            return Err(refused(format!(
                "the socket {} is {} bytes long; a Unix socket's path here is at most {longest}",
                checked.socket,
                checked.socket.len()
            )));
        }
        let mut env = checked.env;
        env.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for pair in env.windows(2) {
            if pair[0].0 == pair[1].0 {
                // alloc: error path, a unit refused once per `blaze daemon install`.
                return Err(refused(format!("the variable {} is given twice", pair[0].0)));
            }
        }
        for &(name, value) in &env {
            let mut chars = name.chars();
            let named = chars.next().is_some_and(|it| it.is_ascii_alphabetic() || it == '_')
                && chars.all(|it| it.is_ascii_alphanumeric() || it == '_');
            if !named {
                // alloc: error path, a unit refused once per `blaze daemon install`.
                return Err(refused(format!("{name:?} is not a variable's name")));
            }
            plain(name, value)?;
        }
        Ok(Checked { env, ..checked })
    }
}

/// `text` holds no control character, which no unit file can hold.
fn plain(what: &str, text: &str) -> Result<(), UnitError> {
    match text.chars().find(|it| it.is_control()) {
        // alloc: error path, a unit refused once per `blaze daemon install`.
        Some(bad) => Err(refused(format!("{what} {text:?} holds the control character {bad:?}"))),
        None => Ok(()),
    }
}

fn plist(unit: &Checked<'_>) -> String {
    // alloc: the file's text, once per `blaze daemon install`.
    let mut out = String::with_capacity(2048);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    let _ = write!(out, "<!-- {WRITTEN} -->\n");
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    key_string(&mut out, 1, "Label", LABEL);
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in [unit.program, "serve", "--activated", "--socket", unit.socket] {
        out.push_str("\t\t<string>");
        xml(&mut out, arg);
        out.push_str("</string>\n");
    }
    out.push_str("\t</array>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    for &(name, value) in &unit.env {
        key_string(&mut out, 2, name, value);
    }
    out.push_str("\t</dict>\n\t<key>Sockets</key>\n\t<dict>\n");
    let _ = write!(out, "\t\t<key>{SOCKET}</key>\n\t\t<dict>\n");
    key_string(&mut out, 3, "SockPathName", unit.socket);
    out.push_str("\t\t\t<key>SockPathMode</key>\n\t\t\t<integer>384</integer>\n");
    out.push_str("\t\t</dict>\n\t</dict>\n");
    out.push_str("\t<key>RunAtLoad</key>\n\t<false/>\n");
    key_string(&mut out, 1, "ProcessType", "Interactive");
    let _ = write!(out, "\t<key>ThrottleInterval</key>\n\t<integer>{THROTTLE_SECS}</integer>\n");
    key_string(&mut out, 1, "StandardOutPath", unit.log);
    key_string(&mut out, 1, "StandardErrorPath", unit.log);
    out.push_str("</dict>\n</plist>\n");
    out
}

fn key_string(out: &mut String, depth: usize, key: &str, value: &str) {
    let tabs = &"\t\t\t\t"[..depth];
    for (open, text, close) in [("<key>", key, "</key>\n"), ("<string>", value, "</string>\n")] {
        out.push_str(tabs);
        out.push_str(open);
        xml(out, text);
        out.push_str(close);
    }
}

fn socket_unit(unit: &Checked<'_>) -> String {
    // alloc: the file's text, once per `blaze daemon install`.
    let mut out = String::with_capacity(512);
    let _ = write!(out, "# {WRITTEN}\n[Unit]\nDescription=blaze's daemon socket\n\n[Socket]\nListenStream=");
    escaped(&mut out, unit.socket, Escape::Specifiers);
    let _ = write!(
        out,
        "\n\
         FileDescriptorName={SOCKET}\n\
         SocketMode=0600\n\
         DirectoryMode=0700\n\
         RemoveOnStop=yes\n\
         \n\
         [Install]\n\
         WantedBy=sockets.target\n"
    );
    out
}

fn service_unit(unit: &Checked<'_>) -> String {
    // alloc: the file's text, once per `blaze daemon install`.
    let mut out = String::with_capacity(1024);
    let _ = write!(
        out,
        "# {WRITTEN}\n\
         [Unit]\n\
         Description=blaze's daemon (blaze serve), started by {SOCKET_UNIT}\n\
         Requires={SOCKET_UNIT}\n\
         After={SOCKET_UNIT}\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart="
    );
    for (at, arg) in [unit.program, "serve", "--activated", "--socket", unit.socket].into_iter().enumerate() {
        if at > 0 {
            out.push(' ');
        }
        word(&mut out, arg);
    }
    out.push('\n');
    for &(name, value) in &unit.env {
        out.push_str("Environment=\"");
        for text in [name, "=", value] {
            escaped(&mut out, text, Escape::Quoted);
        }
        out.push_str("\"\n");
    }
    for stream in ["StandardOutput", "StandardError"] {
        let _ = write!(out, "{stream}=append:");
        escaped(&mut out, unit.log, Escape::Specifiers);
        out.push('\n');
    }
    out.push_str("KillMode=mixed\n");
    out
}

/// What [`escaped`] escapes besides systemd's specifiers (`%`, doubled always).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Specifiers,
    /// In double quotes: `\` and `"` too.
    Quoted,
    /// A word of a command line, which systemd expands variables in: `$` too.
    Word,
    /// A word of a command line in double quotes.
    QuotedWord,
}

/// `text` onto `out` as systemd reads it back as `text`.
fn escaped(out: &mut String, text: &str, escape: Escape) {
    let quoted = matches!(escape, Escape::Quoted | Escape::QuotedWord);
    let word = matches!(escape, Escape::Word | Escape::QuotedWord);
    for it in text.chars() {
        match it {
            '%' => out.push_str("%%"),
            '$' if word => out.push_str("$$"),
            '\\' if quoted => out.push_str("\\\\"),
            '"' if quoted => out.push_str("\\\""),
            other => out.push(other),
        }
    }
}

/// One word of an `ExecStart=` line: as it is when nothing in it is special, else quoted.
/// `$` is doubled either way, as systemd expands variables in command lines.
fn word(out: &mut String, text: &str) {
    let special = text.is_empty() || text.chars().any(|it| it.is_whitespace() || matches!(it, '"' | '\'' | '\\' | ';'));
    match special {
        true => {
            out.push('"');
            escaped(out, text, Escape::QuotedWord);
            out.push('"');
        }
        false => escaped(out, text, Escape::Word),
    }
}

/// `text` onto `out` with XML's five special characters as entities.
fn xml(out: &mut String, text: &str) {
    for it in text.chars() {
        match it {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
}

/// What [`xml`] wrote, read back; `None` for an entity it never writes.
fn xml_text(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let (entity, after) = rest[at..].split_once(';')?;
        out.push(match entity {
            "&amp" => '&',
            "&lt" => '<',
            "&gt" => '>',
            "&quot" => '"',
            "&apos" => '\'',
            _ => return None,
        });
        rest = after;
    }
    out.push_str(rest);
    Some(out)
}

fn launchd_names(place: &Place) -> (String, String) {
    // alloc: the names, once per `blaze daemon install` or `uninstall`.
    let domain = format!("gui/{}", place.uid);
    // alloc: as above.
    let service = format!("{domain}/{LABEL}");
    (domain, service)
}

fn words(argv: &[&str]) -> Vec<String> {
    // alloc: a command's words, a few per `blaze daemon install` or `uninstall`.
    argv.iter().map(|it| (*it).to_owned()).collect()
}

#[cfg(test)]
mod tests;
