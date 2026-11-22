//! What a step that runs the set is given: its guest folder and `PATH`, its fixed environment,
//! the Seatbelt lines for the set's host folder, the guard that a whole profile grants no tool
//! folder of the machine, and the `etc/answers` file the set's wrappers read.
//!
//! Guest and host. A step's key, records and environment spell the set at [`GUEST_ROOT`]
//! (`/tools`, its programs at [`GUEST_BIN`]): one spelling on every machine (rule 25). The
//! sandbox runs it from the set's folder on this machine's shelf ([`HostRoot`]); the runner
//! puts the pair `(GUEST_ROOT, host root)` among its guest/host folder pairs, so `PATH` and
//! every argument are made the host's when the step starts and the host's are made the guest's
//! again in what it leaves (the runner's existing guest/host mapping; Seatbelt matches real
//! paths, so the profile names the host's).
//!
//! The answers file ([`ANSWERS_MEMBER`], `etc/answers` in the archive) is lines of
//! `<key>=<value>\n`, sorted by key, ASCII, nothing else (no comment, no blank line, no
//! trailing blanks); a value runs to the line's end and may hold `=`. The keys are exactly
//! `hostname` and `uname.m`, `uname.n`, `uname.o`, `uname.p`, `uname.r`, `uname.s`, `uname.v`
//! (`uname.n` is the hostname). The set's `hostname` prints `hostname`'s value and a newline;
//! its `uname` prints the values its flags ask for, in `uname`'s order, space-separated (`-a`
//! all of them), and refuses a flag it has no answer for. Its `date` answers from
//! `SOURCE_DATE_EPOCH` alone (the step's, [`step_env`]), in UTC: with no operand it prints that
//! time in the format asked (`+FORMAT`, or the default); setting the clock or a missing
//! `SOURCE_DATE_EPOCH` is a failure that says so. None of the three reads the machine.

use core::fmt;

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::pins::Answers;

/// Where a step sees the set: the same folder on every machine.
pub const GUEST_ROOT: &str = "/tools";
/// The set's programs as the step sees them: the step's whole `PATH`.
pub const GUEST_BIN: &str = "/tools/bin";
/// The archive's member that holds the wrappers' answers ([`answers_file`]).
pub const ANSWERS_MEMBER: &str = "etc/answers";
/// The archive's member that lists every other member with its SHA-256 and mode.
pub const MANIFEST_MEMBER: &str = "MANIFEST";

/// The folders of the machine's own tools (and of Apple's developer tools): a set's host folder
/// is never in one, and a profile grants nothing in one outside the set and the SDK
/// ([`os_tool_in_profile`]).
pub const OS_TOOL_FOLDERS: [&str; 10] = [
    "/Applications",
    "/Library/Developer",
    "/bin",
    "/opt",
    "/private/var/select",
    "/sbin",
    "/usr/bin",
    "/usr/libexec",
    "/usr/local",
    "/usr/sbin",
];

/// The folders a set's host folder is never under besides [`OS_TOOL_FOLDERS`]: the system's
/// own.
const SYSTEM_FOLDERS: [&str; 6] = ["/System", "/Library", "/dev", "/etc", "/private/etc", "/usr"];

/// Words that name Apple's developer tools wherever they are installed.
const DEVELOPER_WORDS: [&str; 2] = ["Xcode", "CommandLineTools"];

/// Whether `path` is `folder` or under it.
fn within(path: &str, folder: &str) -> bool {
    path.strip_prefix(folder).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether `path` is a folder above `below` (`/a` above `/a/b`).
fn above(path: &str, below: &str) -> bool {
    path == "/" || below.strip_prefix(path).is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `path` is one of the machine's tool folders or under one, or names Apple's
/// developer tools.
fn os_tool_path(path: &str) -> bool {
    OS_TOOL_FOLDERS.iter().any(|folder| within(path, folder)) || DEVELOPER_WORDS.iter().any(|word| path.contains(word))
}

/// The set's folder on this machine: its real path (Seatbelt matches real paths), absolute,
/// canonical, sayable in a profile, and in no folder of the machine's own tools or system.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostRoot<'h> {
    path: &'h str,
}

/// A host folder refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostRootError<'h> {
    /// Not absolute, the root itself, or with an empty, `.` or `..` part, or a trailing `/`.
    NotCanonical { path: &'h str },
    /// A quote, backslash or control character: a profile string cannot hold it.
    Unsayable { path: &'h str },
    /// In a folder of the machine's own tools or system, or named for Apple's developer tools.
    OsFolder { path: &'h str },
}

impl fmt::Display for HostRootError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostRootError::NotCanonical { path } => write!(f, "{path}: the base tool set's folder must be an absolute real path without `.` or `..` (D192)"),
            HostRootError::Unsayable { path } => write!(f, "{path}: a quote, backslash or control character cannot be put in the sandbox's profile (D192)"),
            HostRootError::OsFolder { path } => write!(f, "{path}: the base tool set's folder is in a folder of the machine's own tools or system (D192)"),
        }
    }
}

impl core::error::Error for HostRootError<'_> {}

impl<'h> HostRoot<'h> {
    pub fn new(path: &'h str) -> Result<HostRoot<'h>, HostRootError<'h>> {
        if path.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
            return Err(HostRootError::Unsayable { path });
        }
        let Some(rest) = path.strip_prefix('/') else { return Err(HostRootError::NotCanonical { path }) };
        if rest.is_empty() || rest.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(HostRootError::NotCanonical { path });
        }
        if os_tool_path(path) || SYSTEM_FOLDERS.iter().any(|folder| within(path, folder)) {
            return Err(HostRootError::OsFolder { path });
        }
        Ok(HostRoot { path })
    }

    pub fn path(&self) -> &'h str {
        self.path
    }

    /// The pair the runner maps guest spellings by: `(GUEST_ROOT, the host folder)`.
    pub fn guest_pair(&self) -> (&'static str, &'h str) {
        (GUEST_ROOT, self.path)
    }
}

/// `SOURCE_DATE_EPOCH`: the step's one time, in seconds since 1970, handed in by the caller
/// (the commit's or the record's; never the clock: rule 75).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SourceDateEpoch(pub u64);

/// The pinned variables of a step that runs the set, sorted by name: `LC_ALL=C`,
/// `PATH=/tools/bin` (the set's programs and nothing else), `SOURCE_DATE_EPOCH`, `TZ=UTC`. The
/// runner sets them over the step's captured environment (base-tools.md, "Rules"); `PATH` is
/// spelled at the guest folder (the runner's guest/host pairs make it the host's at the start).
pub fn step_env<'a>(epoch: SourceDateEpoch, alloc: Alloc<'a>) -> AVec<'a, (&'static str, AString<'a>)> {
    let text = |value: &str| String::from_str_in(value, alloc);
    let mut seconds: AString<'a> = String::new_in(alloc);
    blaze_own::format_into!(&mut seconds, "{}", epoch.0);
    let mut out: AVec<'a, (&'static str, AString<'a>)> = Vec::with_capacity_in(4, alloc);
    out.push(("LC_ALL", text("C")));
    out.push(("PATH", text(GUEST_BIN)));
    out.push(("SOURCE_DATE_EPOCH", seconds));
    out.push(("TZ", text("UTC")));
    out
}

fn line(out: &mut AString<'_>, op: &str, filter: &str, path: &str) {
    out.push_str("(allow ");
    out.push_str(op);
    out.push_str(" (");
    out.push_str(filter);
    out.push_str(" \"");
    out.push_str(path);
    out.push_str("\"))\n");
}

/// The Seatbelt lines that let a step start and read the set at `root`, in bstd's hermetic
/// profile's spelling (blaze-std procs/hermetic/seatbelt.rs `profile`: one rule a line; this is
/// its rule `{ path: root, subtree: true, access: Exec }`): start and read anything under the
/// set's folder (the programs, clang's resource headers, perl's and python's libraries, the
/// answers), and the metadata of each folder above it (a path's resolution stats each one).
/// Nothing under `/bin`, `/usr/bin` or a developer-tools folder: [`HostRoot`] cannot be one.
pub fn profile_lines<'a>(root: &HostRoot<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let path = root.path;
    let mut out: AString<'a> = String::new_in(alloc);
    line(&mut out, "process-exec", "subpath", path);
    line(&mut out, "file-read*", "subpath", path);
    for (at, _) in path.match_indices('/').filter(|(at, _)| *at > 0) {
        line(&mut out, "file-read-metadata", "literal", &path[..at]);
    }
    out
}

/// The folders a profile may name although they hold the machine's tools: the set's own, and
/// the Apple SDK's (read, as [`crate::FromMachine::AppleSdk`]) when the step reads one.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ProfileAllows<'p> {
    pub set_root: &'p HostRoot<'p>,
    pub sdk_root: Option<&'p str>,
}

/// A profile line that grants a folder of the machine's tools.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OsToolGrant<'p> {
    /// 1-based, in the profile.
    pub line: usize,
    pub path: &'p str,
}

impl fmt::Display for OsToolGrant<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the sandbox's profile grants {} (line {}), a folder of the machine's own tools: a step runs only the pinned base tool set (D192)",
            self.path, self.line
        )
    }
}

impl core::error::Error for OsToolGrant<'_> {}

/// The first path a whole profile grants in a folder of the machine's tools
/// ([`OS_TOOL_FOLDERS`], or a path naming Apple's developer tools) outside the folders `allows`
/// names: the guard a runner holds its assembled profile to before it starts anything. Inside
/// the set's folder or the SDK's anything goes; the metadata (`file-read-metadata` of a
/// `literal`) of a folder above either is not a grant of what is in it. Every quoted string of
/// the profile is a path (a profile string holds no quote).
pub fn os_tool_in_profile<'p>(profile: &'p str, allows: ProfileAllows<'_>) -> Result<(), OsToolGrant<'p>> {
    let set = allows.set_root.path;
    let inside = |path: &str| within(path, set) || allows.sdk_root.is_some_and(|sdk| within(path, sdk));
    let over = |path: &str| above(path, set) || allows.sdk_root.is_some_and(|sdk| above(path, sdk));
    for (at, text) in profile.lines().enumerate() {
        let metadata = text.trim_start().starts_with("(allow file-read-metadata");
        // Each quoted string is an odd piece of the line split at its quotes; the word before
        // its opening quote is its filter.
        let mut pieces = text.split('"');
        let mut before = pieces.next().unwrap_or("");
        while let Some(path) = pieces.next() {
            let literal = before.trim_end().ends_with("(literal");
            if !inside(path) && !(metadata && literal && over(path)) && os_tool_path(path) {
                return Err(OsToolGrant { line: at + 1, path });
            }
            before = pieces.next().unwrap_or("");
        }
    }
    Ok(())
}

/// An answer that cannot be a line of `etc/answers`: empty, or not printable ASCII.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BadAnswer<'t> {
    pub key: &'static str,
    pub value: &'t str,
}

impl fmt::Display for BadAnswer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the answer {} = {:?} is not one line of printable ASCII (D192)", self.key, self.value)
    }
}

impl core::error::Error for BadAnswer<'_> {}

/// The `etc/answers` file of a set whose answers are `answers` (the module's doc states the
/// format): what the archive's build writes, so the file's bytes, and the set's digest, follow
/// from the pins file. An answer the file cannot hold is refused (the pins reader refuses it
/// first; a hand-made [`Answers`] meets it here).
pub fn answers_file<'a, 't>(answers: &Answers<'t>, alloc: Alloc<'a>) -> Result<AString<'a>, BadAnswer<'t>> {
    let uname = &answers.uname;
    let lines = [
        ("hostname", answers.hostname),
        ("uname.m", uname.m),
        ("uname.n", answers.hostname),
        ("uname.o", uname.o),
        ("uname.p", uname.p),
        ("uname.r", uname.r),
        ("uname.s", uname.s),
        ("uname.v", uname.v),
    ];
    let mut out: AString<'a> = String::new_in(alloc);
    for (key, value) in lines {
        if value.is_empty() || !value.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return Err(BadAnswer { key, value });
        }
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    Ok(out)
}
