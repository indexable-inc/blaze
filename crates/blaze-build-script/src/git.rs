//! The `git` a script spawns, answered by blaze from the checkout, read-only (decision D53 in
//! docs/v2/plan/design/build-scripts.md). Sans-IO: the files of `.git` come through
//! [`GitFiles`], and each is a tracked read of the run's record (an absent one too), so a new
//! commit or checkout reruns exactly the scripts that asked.
//!
//! Answered, shaped like the `rustc` table of [`crate::probe`]:
//! - `git --version`: the fixed [`VERSION`], a stated string, not a real git's;
//! - `rev-parse HEAD` (and `--verify HEAD`): the commit HEAD names, through a loose ref, a
//!   packed ref or a detached head, in a checkout or a linked worktree (`gitdir:` and
//!   `commondir` followed inside the served tree);
//! - `rev-parse --is-inside-work-tree`: `true` when a `.git` is found from the script's folder up;
//! - `rev-parse --git-dir` and `--git-common-dir`: the folder, relative to the one asked in
//!   ([`relative_to`]);
//! - `log -1` and `show -s` of HEAD with a `--pretty=format:` or `--format=` string of the
//!   placeholders [`Placeholder`] names;
//! - with no `.git` from the script's folder up to the served tree's root: every question but
//!   `--version` gets git's own failure, exit 128 and [`NOT_A_REPOSITORY`];
//! - the branch: `rev-parse --abbrev-ref [--symbolic-full-name] HEAD`, `branch --show-current`,
//!   `symbolic-ref HEAD` (on a detached head: git's own exit 128 and message).
//!
//! - `status --porcelain` (with `-uno` or `-uall`) and `describe --dirty`: from the index, a stat
//!   of each tracked file and the ignore files; after a change, from the previous answer and the
//!   journal's paths ([`status_after`]);
//!
//! Refused by name ([`Refused`]), never answered with a guess:
//! - whether the tree is dirty asked another way (`diff-index`, another `status` form, a
//!   `--dirty=<mark>` other than `-dirty`);
//! - an abbreviated commit (`--short`, with or without a length): git lengthens the prefix until
//!   it is unique among the repository's objects, so its length needs the object store;
//! - every other question (`describe`, `log`, `rev-list`, ...): "cannot answer yet".
//!
//! The clock is not git's to answer: a build timestamp is the sandbox's fixed instant.

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// What `git --version` prints, without its newline: blaze's fixed answer, not a real git's.
pub const VERSION: &str = "git version 2.51.0";

/// The checkout's files as the sandbox serves them.
pub trait GitFiles {
    /// Appends the bytes of the file `rel` (`/`-separated, from the served tree's root) to
    /// `into`; `false` when it is absent or a folder. Every call is a read the run's record
    /// lists, an absent file too, so creating it later reruns the script.
    fn read(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> bool;
    /// Appends `len` bytes of the file `rel` from byte `at`; `false` when absent or shorter.
    /// (A pack is read by the ranges of the entries asked for, not whole.)
    fn read_range(&mut self, rel: &str, at: u64, len: u64, into: &mut AVec<'_, u8>) -> bool;
    /// The file `rel`'s stat, not following a symlink; `None` when absent.
    fn stat(&mut self, rel: &str) -> Option<FileStat>;
    /// The names in the folder `rel`, sorted by bytes, into `into`; nothing when it is absent.
    fn list<'a>(&mut self, rel: &str, alloc: Alloc<'a>, into: &mut AVec<'a, AString<'a>>);
    /// `path`, absolute as a worktree's `.git` file names it, from the served tree's root;
    /// `None` when it is outside the tree.
    fn within<'p>(&self, path: &'p str) -> Option<&'p str>;
    /// Appends the target text of the symlink `rel` (its own bytes, as `readlink` gives them) to
    /// `into`. The link is never followed: git stores a link as a blob of exactly this text
    /// (`ce_compare_link`), so its target may name anything, inside the tree or not. A reader
    /// that cannot read links keeps this default, and `status` refuses by name.
    fn read_link(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> Result<(), LinkUnread> {
        let _ = (rel, into);
        Err(LinkUnread::ReaderCannot)
    }
}

/// Why [`GitFiles::read_link`] gave no target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkUnread {
    /// `rel` is absent or not a symlink (it changed since its stat).
    NotALink,
    /// The link is there and `readlink` failed (permissions, or an I/O error).
    Unreadable,
    /// This reader of the checkout does not serve link targets.
    ReaderCannot,
}

pub use crate::git_attributes::Attribute;
pub use crate::git_objects::{After, CoveredSince, Gen, Made, StatusAnswer, StatusPlant, WholeWhy};
pub use blaze_git_fetch::Oid;
pub use blaze_git_fetch::store::{CommitWhy, StoreWhy};

/// What a file is, as `status` compares it with an index entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// A file's stat as `status` compares it with an index entry (times as seconds and
/// nanoseconds since the epoch).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileStat {
    pub kind: FileKind,
    pub size: u64,
    pub mtime: (i64, u32),
    pub ctime: (i64, u32),
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub executable: bool,
}

/// One piece of a `log --pretty=format:` string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Piece {
    Byte(u8),
    Placeholder(Placeholder),
}

/// The placeholders the three units ask for (D54); any other is refused naming it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placeholder {
    /// `%cI`
    CommitterIso,
    /// `%cs`
    CommitterShort,
    /// `%ct`
    CommitterSeconds,
    /// `%H`
    Hash,
}

/// The most pieces of a `log` format string read.
pub const MOST_FORMAT: usize = 32;

/// A `log --pretty=format:` string, read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LogFormat {
    pieces: [Piece; MOST_FORMAT],
    len: u8,
    /// `tformat:` (what `--format=<f>` means): a newline after the commit. `format:` separates
    /// commits instead, so the one commit gets none.
    terminated: bool,
}

impl LogFormat {
    /// `text` read as `--pretty=format:<text>`: plain bytes and the [`Placeholder`]s; an
    /// unknown placeholder is refused naming its first two bytes, a longer string as
    /// [`Refused::FormatTooLong`].
    pub fn parse(text: &str) -> Result<LogFormat, Refused> {
        LogFormat::read(text, false)
    }

    /// `text` read as `--format=<text>`, which is `--pretty=tformat:<text>` (git-log(1),
    /// "PRETTY FORMATS"): the same pieces, then a newline.
    pub fn parse_terminated(text: &str) -> Result<LogFormat, Refused> {
        LogFormat::read(text, true)
    }

    fn read(text: &str, terminated: bool) -> Result<LogFormat, Refused> {
        let mut out = LogFormat { pieces: [Piece::Byte(0); MOST_FORMAT], len: 0, terminated };
        let bytes = text.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if usize::from(out.len) == MOST_FORMAT {
                return Err(Refused::FormatTooLong);
            }
            let piece = if bytes[at] == b'%' {
                let rest = &bytes[at + 1..];
                let (placeholder, used) = match rest {
                    [b'c', b'I', ..] => (Placeholder::CommitterIso, 2),
                    [b'c', b's', ..] => (Placeholder::CommitterShort, 2),
                    [b'c', b't', ..] => (Placeholder::CommitterSeconds, 2),
                    [b'H', ..] => (Placeholder::Hash, 1),
                    _ => return Err(Refused::Placeholder([rest.first().copied().unwrap_or(0), rest.get(1).copied().unwrap_or(0)])),
                };
                at += 1 + used;
                Piece::Placeholder(placeholder)
            } else {
                at += 1;
                Piece::Byte(bytes[at - 1])
            };
            out.pieces[usize::from(out.len)] = piece;
            out.len += 1;
        }
        Ok(out)
    }

    pub fn pieces(&self) -> &[Piece] {
        &self.pieces[..usize::from(self.len)]
    }

    /// Whether a newline follows the commit ([`LogFormat::parse_terminated`]).
    pub fn terminated(&self) -> bool {
        self.terminated
    }
}

/// Where a script's `git` answers come from (D54).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vcs {
    /// A `.git` in the served tree, from the script's folder up (a workspace checkout). A tree
    /// with none (a registry crate, a checkout copied without its `.git`) gets git's own answer
    /// outside a repository, [`NOT_A_REPOSITORY`] with exit 128.
    Served,
    /// A fetched git source: the lock's commit, whose tree is exactly the served files
    /// (digest-verified at fetch), with no `.git` and no history beyond the commit.
    Fetched(Oid),
}

/// What a `git` spawn asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Question {
    Version,
    InsideWorkTree,
    /// The full commit HEAD names.
    Head,
    /// `rev-parse --git-dir`: the repository's own git folder, as a path from the folder asked
    /// in ([`relative_to`]).
    GitDir,
    /// `rev-parse --git-common-dir`: the folder holding refs and objects (a linked worktree's
    /// `commondir`), from the folder asked in.
    CommonDir,
    /// The branch's short name, `HEAD` when detached.
    Branch,
    /// The branch's short name, empty when detached.
    ShowCurrent,
    /// The full name of the ref HEAD points to.
    SymbolicRef,
    /// An abbreviated commit (`--short`).
    Short,
    /// `status --porcelain`, listing untracked files when `untracked` (git's default) and not
    /// with `--untracked-files=no`; `all` selects `--untracked-files=all` instead of Git's
    /// directory-collapsed default.
    Status { untracked: bool, all: bool },
    /// Whether the tree is dirty, asked another way (`diff-index`, another `status` form, a
    /// `describe --dirty=<mark>` with a mark other than git's own `-dirty`).
    Dirty,
    /// `log -1` with a `--pretty=format:` or `--format=` string, or `show -s` (`--no-patch`)
    /// with one, of HEAD: the HEAD commit object. The format as read, or why it is refused.
    Log(Result<LogFormat, Refused>),
    /// `describe` with only `--always`, `--tags`, `--match` and `--dirty`: a walk of history to
    /// a tag; with `always`, the abbreviated commit when no tag is found; with `dirty`, `-dirty`
    /// after it when a tracked file differs ([`dirty_marked`]: the tracked status not empty).
    Describe { always: bool, dirty: bool },
    /// Anything else.
    Other,
}

/// Reads `git`'s arguments (the program name not among them).
pub fn question(args: &[&str]) -> Question {
    match args {
        ["--version"] | ["version"] => Question::Version,
        ["rev-parse", "--is-inside-work-tree"] => Question::InsideWorkTree,
        ["rev-parse", "HEAD"] | ["rev-parse", "--verify", "HEAD"] => Question::Head,
        ["rev-parse", "--git-dir"] => Question::GitDir,
        ["rev-parse", "--git-common-dir"] => Question::CommonDir,
        ["rev-parse", "--abbrev-ref", "HEAD"]
        | ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "HEAD"]
        | ["rev-parse", "--symbolic-full-name", "--abbrev-ref", "HEAD"] => Question::Branch,
        ["branch", "--show-current"] => Question::ShowCurrent,
        ["symbolic-ref", "HEAD"] => Question::SymbolicRef,
        ["rev-parse", rest @ ..] if rest.iter().any(|arg| *arg == "--short" || arg.starts_with("--short=")) => Question::Short,
        ["status", "--porcelain"] => Question::Status { untracked: true, all: false },
        ["status", "--porcelain", "--untracked-files=no"] | ["status", "--porcelain", "-uno"] => Question::Status { untracked: false, all: false },
        ["status", "--porcelain", "--untracked-files=all"] | ["status", "--porcelain", "-uall"] => Question::Status { untracked: true, all: true },
        ["status", ..] | ["diff-index", ..] => Question::Dirty,
        ["describe", rest @ ..] => describe(rest),
        ["log", "-1", format] => log_format(format),
        // `show` of one commit with its diff off: the commit's line in the format (turso_core's
        // build.rs: `show -s --format=%ct HEAD`).
        ["show", "-s" | "--no-patch", format] | ["show", "-s" | "--no-patch", format, "HEAD"] => log_format(format),
        _ => Question::Other,
    }
}

/// A `log`/`show` format argument: `--pretty=format:<f>` (no newline after the one commit),
/// `--format=<f>` (`--pretty=tformat:<f>`: a newline after it); any other is
/// [`Question::Other`].
fn log_format(format: &str) -> Question {
    if let Some(text) = format.strip_prefix("--pretty=format:") {
        Question::Log(LogFormat::parse(text))
    } else if let Some(text) = format.strip_prefix("--format=") {
        Question::Log(LogFormat::parse_terminated(text))
    } else {
        Question::Other
    }
}

/// git's own answer to a question that needs a repository, asked where there is none (setup.c
/// `setup_git_directory_gently`: `die(_("not a git repository (or any of the parent
/// directories): %s"), DEFAULT_GIT_DIR_ENVIRONMENT)`), exit 128.
pub const NOT_A_REPOSITORY: &str = "fatal: not a git repository (or any of the parent directories): .git\n";

/// `target` (from the served tree's root) as a path from `folder` (from the same root): `..`
/// per part of `folder` below the parts they share, then the rest of `target`; `.` when they are
/// one folder. git prints `--git-dir` relative "to the current working directory" (git-rev-parse(1));
/// at a worktree's top that is `.git`, exactly git's answer. Below the top git prints the
/// absolute path, which in the sandbox is the mount (`/workspace/..`), not the host's; the
/// relative form names the same folder from the script's folder and holds nothing of the
/// machine (rule 25), so it is the answer in every folder.
pub fn relative_to<'a>(folder: &str, target: &str, alloc: Alloc<'a>) -> AString<'a> {
    let parts = |path: &'_ str| path.split('/').filter(|part| !part.is_empty()).count();
    let mut from = folder.split('/').filter(|part| !part.is_empty()).peekable();
    let mut to = target.split('/').filter(|part| !part.is_empty()).peekable();
    let mut shared = 0;
    while let (Some(a), Some(b)) = (from.peek(), to.peek()) {
        if a != b {
            break;
        }
        from.next();
        to.next();
        shared += 1;
    }
    let mut out = String::new_in(alloc);
    for _ in shared..parts(folder) {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str("..");
    }
    for part in to {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

/// The most bytes of a `sh -c` command read as one `git` call; a longer one is refused as the
/// shell it is.
pub const MOST_SHELL_COMMAND: usize = 4096;

/// The paths of `.git/index`'s entries (`bytes`), in the index's order: what `status` stats,
/// one each. Read as `status` reads them, refusals included.
pub fn index_paths<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<AVec<'a, AString<'a>>, Refused> {
    let index = crate::git_objects::parse_index(bytes, alloc)?;
    let mut paths = Vec::new_in(alloc);
    for entry in index.entries.iter() {
        paths.push(String::from_str_in(&entry.path, alloc));
    }
    Ok(paths)
}

/// The words of `sh -c <command>` after `git` when the command is one plain `git` call. Words
/// split at spaces; a `'...'` part is literal; a `"..."` part is literal when it holds no `$`,
/// `` ` ``, `\` or `!` (the only characters POSIX `sh` treats specially inside double quotes,
/// with `!` for bash's history); parts join into one word (`--pretty=format:'%cI'` is
/// `--pretty=format:%cI`). Outside quotes no expansion, glob, redirection or second command is
/// read. `None` for anything else (or a command over [`MOST_SHELL_COMMAND`] bytes), which the
/// sandbox refuses as the shell it is.
pub fn shell_git<'a>(command: &str, alloc: Alloc<'a>) -> Option<AVec<'a, AString<'a>>> {
    const SPECIAL: &[char] = &[
        '\\', '$', '`', ';', '&', '|', '<', '>', '(', ')', '{', '}', '*', '?', '[', ']', '~', '#', '!', '\n',
    ];
    const IN_DOUBLE: &[char] = &['$', '`', '\\', '!'];
    if command.len() > MOST_SHELL_COMMAND {
        return None;
    }
    let mut words: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let mut word: Option<AString<'a>> = None;
    let mut rest = command;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        match c {
            ' ' | '\t' => {
                if let Some(done) = word.take() {
                    words.push(done);
                }
            }
            '\'' | '"' => {
                let end = rest.find(c)?;
                let part = &rest[..end];
                if c == '"' && part.contains(IN_DOUBLE) {
                    return None;
                }
                word.get_or_insert_with(|| String::new_in(alloc)).push_str(part);
                rest = &rest[end + 1..];
            }
            c if SPECIAL.contains(&c) => return None,
            c => word.get_or_insert_with(|| String::new_in(alloc)).push(c),
        }
    }
    if let Some(done) = word.take() {
        words.push(done);
    }
    if words.first().map(|first| first.as_str()) != Some("git") {
        return None;
    }
    words.remove(0);
    Some(words)
}

/// A file of `.git` a refusal is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitFile {
    Head,
    /// A `.git` file's `gitdir: <path>` (a linked worktree or a submodule).
    Gitdir,
    Commondir,
    LooseRef,
    PackedRefs,
    /// `objects/info/alternates`
    Alternates,
    /// A name in `objects/pack` that is `pack-<not an id>.idx`.
    PackName,
    /// `index`, absent or unreadable.
    Index,
}

/// An object-store file, named by the id in its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreFile {
    /// `objects/ab/cdef..`
    Loose(Oid),
    /// `objects/pack/pack-<id>.idx`
    Idx(Oid),
    /// `objects/pack/pack-<id>.pack`
    Pack(Oid),
}

/// Why a `git` question is not answered: the script stops, naming the command and this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Whether the tree is dirty, asked in a form not answered ([`Question::Dirty`]).
    Dirty,
    /// An abbreviated commit, whose length git makes unique among the objects.
    Short,
    /// No `.git` from the script's folder up to the served tree's root. [`answer`] turns it into
    /// git's own failure ([`NOT_A_REPOSITORY`]); it stops a script only when the spawn's folder
    /// is outside the served tree (script_sandbox.rs).
    NoRepository,
    /// HEAD names no commit: an unborn branch or a ref in neither a loose file nor `packed-refs`.
    NoCommit,
    /// A file of `.git` that is not what git writes.
    Malformed(GitFile),
    /// A `gitdir:` or `commondir` leading outside the served tree.
    Outside(GitFile),
    /// An object-store file that is not what git writes, at this byte.
    Store { file: StoreFile, at: u64, why: StoreWhy },
    /// A commit object that is not what git writes, at this byte of its body.
    Commit { commit: Oid, at: u64, why: CommitWhy },
    /// An object in no loose file and no pack.
    ObjectMissing(Oid),
    /// An object asked for as a commit is another kind.
    NotCommit(Oid),
    /// A delta chain longer than git's own `pack.depth` ceiling, in this pack.
    DepthOver { pack: Oid },
    /// A `multi-pack-index` (its object count is part of the abbreviation's length; not read).
    MultiPackIndex,
    /// An alternate with alternates of its own (git follows five deep; blaze one).
    NestedAlternates,
    TooManyPacks,
    /// A committer date outside the years 0 to 9999.
    DateOutOfRange,
    /// A `log` format placeholder no unit asks for (its first two bytes).
    Placeholder([u8; 2]),
    FormatTooLong,
    /// `.git/index` is not what git writes, at this byte.
    IndexMalformed { at: u64 },
    /// More index entries, or worktree names walked, than the cap.
    TooManyEntries,
    /// A sparse or split index, or a skip-worktree entry.
    SparseCheckout,
    /// An unmerged entry (a conflict): `status` would print `UU`.
    Conflict,
    /// A path git would print quoted.
    QuotedPath,
    /// The index's cache tree is invalid or not HEAD's tree: staged changes are not proven
    /// absent (walking HEAD's tree against the index is not built).
    StagedUnproven,
    /// `core.filemode` may be off; blaze does not read git's config.
    ConfigNotRead,
    /// A checked-out submodule: its own status counts.
    Submodule,
    /// A tracked file whose type changed (a folder or link where a file was).
    TypeChanged,
    /// A tracked symlink whose stat changed, whose target text could not be read to compare.
    LinkUnread(LinkUnread),
    /// A file holding a CR, to hash with no `text` attribute in force: git then converts by
    /// `core.autocrlf`, which blaze does not read (a global config may set it).
    AttributesNotApplied,
    /// An attributes file names a byte-changing form blaze does not apply (only `* text=auto`
    /// in the top `.gitattributes` or `info/attributes` is applied).
    AttributeNotBuilt(Attribute),
    /// Kept for malformed-reader compatibility. Served status now prints an untracked path after
    /// evaluating the checkout's `.gitignore` files and `info/exclude`.
    Untracked,
    /// An index entry, starting at byte `at`, whose path is absolute or has an empty, `.`, `..`
    /// or `.git` component: git never writes one (`verify_path`).
    IndexPath { at: u64 },
    /// `describe` with tags present: the capped walk of history to a tag is not built.
    DescribeNotBuilt,
    /// A question no row answers yet.
    CannotAnswerYet,
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refused::Dirty => f.write_str("whether the tree is dirty, asked in a form not answered (`diff-index`, a `status` form other than `--porcelain` with `-uno`/`-uall`, a `--dirty=<mark>` other than `-dirty`); `status --porcelain` and `describe --dirty` are"),
            Refused::Short => f.write_str("an abbreviated commit: git lengthens it until it is unique among the repository's objects, which needs the object store"),
            Refused::NoRepository => f.write_str("the script asked from a folder outside the served tree, where no `.git` can be looked for"),
            Refused::NoCommit => f.write_str("HEAD names no commit (an unborn branch, or a ref neither loose nor packed)"),
            Refused::Malformed(file) => write!(f, "the {file:?} file of `.git` is not what git writes"),
            Refused::Outside(file) => write!(f, "the {file:?} file of `.git` leads outside the served tree"),
            Refused::Store { file, at, why } => write!(f, "{file:?} is not what git writes at byte {at}: {why:?}"),
            Refused::Commit { commit, at, why } => write!(f, "the commit {commit} is not what git writes at byte {at} of its body: {why:?}"),
            Refused::ObjectMissing(oid) => write!(f, "the object {oid} is in no loose file and no pack"),
            Refused::NotCommit(oid) => write!(f, "the object {oid} is not a commit"),
            Refused::DepthOver { pack } => write!(f, "a delta chain in pack-{pack} is longer than git's own ceiling of 4095"),
            Refused::MultiPackIndex => f.write_str("a multi-pack-index (it counts toward the abbreviation's length; not read)"),
            Refused::NestedAlternates => f.write_str("an alternate object folder with alternates of its own (followed one level)"),
            Refused::TooManyPacks => f.write_str("more than 1024 packs"),
            Refused::DateOutOfRange => f.write_str("a committer date outside the years 0 to 9999"),
            Refused::Placeholder(bytes) => write!(f, "the `log` placeholder `%{}{}` (answered: %cI %cs %ct %H)", bytes[0] as char, bytes[1] as char),
            Refused::FormatTooLong => f.write_str("a `log` format longer than 32 pieces"),
            Refused::IndexMalformed { at } => write!(f, "`.git/index` is not what git writes at byte {at}"),
            Refused::TooManyEntries => f.write_str("more index entries or worktree names than the cap of 4194304"),
            Refused::SparseCheckout => f.write_str("a sparse or split index, or a skip-worktree entry"),
            Refused::Conflict => f.write_str("an unmerged index entry (a conflict)"),
            Refused::QuotedPath => f.write_str("a path git would print quoted"),
            Refused::IndexPath { at } => write!(f, "`.git/index` names a path outside the worktree or inside `.git` at byte {at}"),
            Refused::StagedUnproven => f.write_str("the index's cache tree is not HEAD's tree: staged changes are not proven absent (walking HEAD's tree is not built)"),
            Refused::ConfigNotRead => f.write_str("`core.filemode` may be off in git's config, which blaze does not read"),
            Refused::Submodule => f.write_str("a checked-out submodule (its own status counts)"),
            Refused::TypeChanged => f.write_str("a tracked file whose type changed"),
            Refused::LinkUnread(LinkUnread::NotALink) => f.write_str("a tracked symlink is no longer a link"),
            Refused::LinkUnread(LinkUnread::Unreadable) => f.write_str("a tracked symlink whose stat changed cannot be read (`readlink` failed)"),
            Refused::LinkUnread(LinkUnread::ReaderCannot) => f.write_str("a tracked symlink whose stat changed, served by a reader that cannot read link targets (`GitFiles::read_link` not given)"),
            Refused::AttributesNotApplied => f.write_str("a file holding a CR, to hash with no `text` attribute in force: git would consult `core.autocrlf`, which blaze does not read"),
            Refused::AttributeNotBuilt(which) => write!(f, "an attributes file names the byte-changing form {which:?}, which blaze does not apply (gitattributes(5); only `* text=auto` in the top `.gitattributes` or `info/attributes` is)"),
            Refused::Untracked => f.write_str("an untracked file was refused by the status reader"),
            Refused::DescribeNotBuilt => f.write_str("`describe` with tags present walks history to a tag; blaze's capped walk (D54) is not built yet"),
            Refused::CannotAnswerYet => f.write_str("blaze cannot answer this git question yet"),
        }
    }
}

/// A `git` answer: the exit code and the two outputs.
#[derive(Debug)]
pub struct Answer<'a> {
    pub code: i32,
    pub stdout: AString<'a>,
    pub stderr: &'static str,
}

impl Answer<'_> {
    /// The answer's bytes as a read's digest hashes them: the exit code, then stdout and stderr,
    /// each length-first (D54: the record lists the question and this, not the files behind it).
    pub fn digest_bytes(&self, out: &mut AVec<'_, u8>) {
        out.extend_from_slice(&self.code.to_le_bytes());
        for part in [self.stdout.as_bytes(), self.stderr.as_bytes()] {
            out.extend_from_slice(&(part.len() as u64).to_le_bytes());
            out.extend_from_slice(part);
        }
    }
}

/// `question`, asked in `folder` (from the served tree's root), answered from `vcs`: a
/// fetched source's commit, or the served tree's `.git` through `files`.
pub fn answer<'a>(question: Question, folder: &str, vcs: Vcs, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<Answer<'a>, Refused> {
    let ok = |text: &str| {
        let mut stdout = String::from_str_in(text, alloc);
        if !text.is_empty() {
            stdout.push('\n');
        }
        Answer { code: 0, stdout, stderr: "" }
    };
    if let Vcs::Fetched(commit) = vcs {
        // The served files are exactly the commit's tree (digest-verified at fetch), and a
        // fetched source is checked out detached at the lock's commit.
        let hex = commit.hex();
        let hex = core::str::from_utf8(&hex).expect("hex digits");
        return match question {
            Question::Version => Ok(ok(VERSION)),
            Question::InsideWorkTree => Ok(ok("true")),
            Question::Head => Ok(ok(hex)),
            Question::Branch => Ok(ok("HEAD")),
            Question::ShowCurrent => Ok(ok("")),
            Question::SymbolicRef => Ok(Answer { code: 128, stdout: String::new_in(alloc), stderr: "fatal: ref HEAD is not a symbolic ref\n" }),
            // Clean: no tracked file differs from the commit, and no file is untracked.
            Question::Status { .. } => Ok(ok("")),
            Question::Short => Err(Refused::Short),
            Question::Log(Err(why)) => Err(why),
            // The record keeps the commit id, not the commit object (D54): git's own failure,
            // which every unit asking tolerates with no value (smolgen: no commit-date keys).
            Question::Log(Ok(_)) => Ok(Answer { code: 128, stdout: String::new_in(alloc), stderr: FETCHED_NO_COMMIT_OBJECT }),
            // D54: a fetched source is a shallow, tagless checkout of the commit, so no tag is
            // reachable: without `--always` git's own failure; with it the abbreviated commit
            // at git's automatic length, `max(7, ceil((msb(n) + 1) / 2))` (object-name.c), which
            // is 7 while the commit's objects number under 2^14.
            // Clean, as for `status`: no `-dirty` mark.
            Question::Describe { always: false, .. } => Ok(Answer { code: 128, stdout: String::new_in(alloc), stderr: NO_NAMES }),
            Question::Describe { always: true, .. } => Ok(ok(&hex[..FALLBACK_DEFAULT_ABBREV])),
            // A fetched source serves no `.git` folder to name: a script would read files under
            // it that are not there.
            Question::GitDir | Question::CommonDir | Question::Dirty | Question::Other => Err(Refused::CannotAnswerYet),
        };
    }
    match question {
        Question::Version => return Ok(ok(VERSION)),
        Question::Short => return Err(Refused::Short),
        Question::Dirty => return Err(Refused::Dirty),
        Question::Other => return Err(Refused::CannotAnswerYet),
        Question::Log(Err(why)) => return Err(why),
        _ => {}
    }
    let repo = match find(folder, files, alloc) {
        Ok(repo) => repo,
        // git's own answer: every question left needs a repository ([`NOT_A_REPOSITORY`]).
        // The served tree's root is the ceiling (as `GIT_CEILING_DIRECTORIES` makes one):
        // nothing above it is served, so nothing above it is looked at. The answer is a fact of
        // the run's record like any other (D54): `find` read `.git` and `.git/HEAD` at each
        // folder up, a command recomputes the answer, and a repository made later changes it,
        // which reruns the script.
        Err(Refused::NoRepository) => return Ok(Answer { code: 128, stdout: String::new_in(alloc), stderr: NOT_A_REPOSITORY }),
        Err(why) => return Err(why),
    };
    match question {
        Question::InsideWorkTree => return Ok(ok("true")),
        Question::GitDir => return Ok(ok(&relative_to(folder, &repo.gitdir, alloc))),
        Question::CommonDir => return Ok(ok(&relative_to(folder, &repo.common, alloc))),
        Question::Log(Ok(format)) => {
            let commit = head_commit(&repo, files, alloc)?;
            let store = crate::git_objects::Store::open(&repo.common, files, alloc)?;
            let mut body = Vec::new_in(alloc);
            crate::git_objects::read_commit(&store, &commit, files, alloc, &mut body)?;
            let parsed = blaze_git_fetch::store::Commit::parse(&body).map_err(|(at, why)| Refused::Commit { commit, at, why })?;
            let mut stdout = String::new_in(alloc);
            let mut run = Vec::new_in(alloc);
            // The bytes between placeholders were cut from a `&str` at ASCII `%`: whole UTF-8.
            let flush = |stdout: &mut AString<'a>, run: &mut AVec<'a, u8>| {
                stdout.push_str(core::str::from_utf8(run).expect("cut at ASCII"));
                run.clear();
            };
            for piece in format.pieces() {
                match *piece {
                    Piece::Byte(byte) => run.push(byte),
                    Piece::Placeholder(which) => {
                        flush(&mut stdout, &mut run);
                        crate::git_objects::placeholder(&mut stdout, which, &parsed, &commit)?;
                    }
                }
            }
            flush(&mut stdout, &mut run);
            // `format:` separates commits: no newline after the one commit; `tformat:`
            // (`--format=`) ends it with one.
            if format.terminated() {
                stdout.push('\n');
            }
            return Ok(Answer { code: 0, stdout, stderr: "" });
        }
        Question::Status { untracked, all } => {
            let tree = head_tree(&repo, files, alloc)?;
            let stdout = crate::git_objects::status(&repo.gitdir, &repo.common, &repo.work, untracked, all, tree, files, alloc)?;
            return Ok(Answer { code: 0, stdout, stderr: "" });
        }
        Question::Describe { always, dirty } => {
            let commit = head_commit(&repo, files, alloc)?;
            if crate::git_objects::has_tags(&repo.common, files, alloc)? {
                return Err(Refused::DescribeNotBuilt);
            }
            if !always {
                return Ok(Answer { code: 128, stdout: String::new_in(alloc), stderr: NO_NAMES });
            }
            // No tag at all: `describe --always` prints the abbreviated commit (git's describe
            // documentation: "Show uniquely abbreviated commit object as fallback").
            let store = crate::git_objects::Store::open(&repo.common, files, alloc)?;
            let len = store.abbrev(&commit, files, alloc)?;
            let hex = commit.hex();
            let answer = ok(core::str::from_utf8(&hex[..len]).expect("hex"));
            if !dirty {
                return Ok(answer);
            }
            let tree = head_tree(&repo, files, alloc)?;
            let tracked = crate::git_objects::status(&repo.gitdir, &repo.common, &repo.work, false, false, tree, files, alloc)?;
            return Ok(dirty_marked(answer, &tracked));
        }
        _ => {}
    }
    let mut bytes = Vec::new_in(alloc);
    if !files.read(&join(&repo.gitdir, "HEAD", alloc), &mut bytes) {
        return Err(Refused::Malformed(GitFile::Head));
    }
    let head = core::str::from_utf8(&bytes).map_err(|_| Refused::Malformed(GitFile::Head))?.trim_end();
    let symbolic = match head.strip_prefix("ref: ") {
        Some(name) if name.starts_with("refs/") => Some(name),
        Some(_) => return Err(Refused::Malformed(GitFile::Head)),
        None if is_commit(head) => None,
        None => return Err(Refused::Malformed(GitFile::Head)),
    };
    let short_branch = |name: &'_ str| -> AString<'a> {
        let mut out = String::from_str_in(name.strip_prefix("refs/heads/").unwrap_or(name), alloc);
        out.push('\n');
        out
    };
    Ok(match (question, symbolic) {
        (Question::Head, Some(name)) => ok(&resolve(&repo, name, files, alloc, 0)?),
        (Question::Head, None) => ok(head),
        (Question::Branch, Some(name)) => {
            resolve(&repo, name, files, alloc, 0)?;
            Answer { code: 0, stdout: short_branch(name), stderr: "" }
        }
        (Question::Branch, None) => ok("HEAD"),
        (Question::ShowCurrent, Some(name)) => Answer { code: 0, stdout: short_branch(name), stderr: "" },
        (Question::ShowCurrent, None) => ok(""),
        (Question::SymbolicRef, Some(name)) => ok(name),
        (Question::SymbolicRef, None) => Answer { code: 128, stdout: String::new_in(alloc), stderr: "fatal: ref HEAD is not a symbolic ref\n" },
        _ => return Err(Refused::CannotAnswerYet),
    })
}

/// git's `describe` with no tag reachable and no `--always` (exit 128).
pub const NO_NAMES: &str = "fatal: No names found, cannot describe anything.\n";

/// git's `FALLBACK_DEFAULT_ABBREV`: the shortest automatic abbreviation (`core.abbrev` unset
/// or `auto`).
pub const FALLBACK_DEFAULT_ABBREV: usize = 7;

/// What a fetched source answers to `log`: git's failure, stated (D54).
pub const FETCHED_NO_COMMIT_OBJECT: &str = "fatal: blaze: a fetched git source keeps no commit object, only its id (D54)\n";

/// `describe`'s arguments read: only `--always`, `--tags` and `--match <glob>` (which change
/// nothing when no tag exists) and `--dirty` (or `--dirty=-dirty`, git's own mark); another
/// mark is [`Question::Dirty`], any other argument [`Question::Other`].
fn describe(args: &[&str]) -> Question {
    let mut always = false;
    let mut dirty = false;
    let mut at = 0;
    while at < args.len() {
        match args[at] {
            "--always" => always = true,
            "--tags" => {}
            "--match" if at + 1 < args.len() => at += 1,
            arg if arg.starts_with("--match=") => {}
            "--dirty" | "--dirty=-dirty" => dirty = true,
            arg if arg.starts_with("--dirty=") => return Question::Dirty,
            _ => return Question::Other,
        }
        at += 1;
    }
    Question::Describe { always, dirty }
}

/// git's `--dirty` mark on a `describe` answer (describe.c: `-dirty` appended when
/// `diff-index HEAD` finds a change, which is a tracked file differing, staged changes being
/// refused by the status): `answer` with `-dirty` before its newline when it succeeded and
/// `tracked_status` (the `status --porcelain --untracked-files=no` text) is not empty. The one
/// home of the mark: [`answer`] and a checkout sweep that answers the status after a change
/// (blaze-cli engine_script_reads.rs) both make it here.
pub fn dirty_marked<'a>(mut answer: Answer<'a>, tracked_status: &str) -> Answer<'a> {
    if answer.code == 0 && !tracked_status.is_empty() {
        if answer.stdout.ends_with('\n') {
            answer.stdout.pop();
        }
        answer.stdout.push_str("-dirty\n");
    }
    answer
}

/// The commit HEAD names, through its ref.
fn head_commit(repo: &Repo<'_>, files: &mut impl GitFiles, alloc: Alloc<'_>) -> Result<Oid, Refused> {
    let mut bytes = Vec::new_in(alloc);
    if !files.read(&join(&repo.gitdir, "HEAD", alloc), &mut bytes) {
        return Err(Refused::Malformed(GitFile::Head));
    }
    let head = core::str::from_utf8(&bytes).map_err(|_| Refused::Malformed(GitFile::Head))?.trim_end();
    let hex = match head.strip_prefix("ref: ") {
        Some(name) if name.starts_with("refs/") => resolve(repo, name, files, alloc, 0)?,
        Some(_) => return Err(Refused::Malformed(GitFile::Head)),
        None => String::from_str_in(head, alloc),
    };
    Oid::parse_hex(hex.as_bytes()).ok_or(Refused::Malformed(GitFile::Head))
}

/// The tree of the commit HEAD names: what the index's cache tree must equal.
fn head_tree(repo: &Repo<'_>, files: &mut impl GitFiles, alloc: Alloc<'_>) -> Result<Oid, Refused> {
    let commit = head_commit(repo, files, alloc)?;
    let store = crate::git_objects::Store::open(&repo.common, files, alloc)?;
    let mut body = Vec::new_in(alloc);
    crate::git_objects::read_commit(&store, &commit, files, alloc, &mut body)?;
    let parsed = blaze_git_fetch::store::Commit::parse(&body).map_err(|(at, why)| Refused::Commit { commit, at, why })?;
    Ok(parsed.tree)
}

/// `status --porcelain` ([`Question::Status`]'s `untracked` and `all`) asked in `folder`,
/// answered from the `previous` answer and the paths the journal named since its place
/// (`covered`, each relative to the served tree's root, as the journal names them), sans-IO
/// through `files` (E3): byte-equal to [`answer`]'s, and computed whole where the change-driven
/// answer is not covered, saying why ([`Made::Whole`]). `now`: the journal's place this command
/// took before reading any file, kept with the answer for the next.
///
/// Each named path is rebased onto the repository's worktree: one under it loses the
/// worktree's prefix, one above it (a lost subtree holding the worktree) names the worktree
/// itself, and one beside it changes nothing of this status.
#[allow(clippy::too_many_arguments, reason = "the question, the previous answer and its proof, the place now, the reader, the allocator and the plant")]
pub fn status_after<'a, F: GitFiles>(
    folder: &str,
    untracked: bool,
    all: bool,
    previous: Option<&StatusAnswer<'_>>,
    covered: Option<&CoveredSince<'_>>,
    now: Gen,
    files: &mut F,
    alloc: Alloc<'a>,
    plant: impl blaze_own::planted::Planted<StatusPlant>,
) -> Result<After<'a>, Refused> {
    let repo = find(folder, files, alloc)?;
    let work = repo.work.as_str();
    let mut rebased: AVec<'_, &str> = Vec::new_in(alloc);
    let mut proof = None;
    if let Some(covered) = covered {
        for path in covered.changed() {
            let rel = if work.is_empty() {
                Some(*path)
            } else if *path == work {
                Some("")
            } else if let Some(rest) = path.strip_prefix(work).and_then(|rest| rest.strip_prefix('/')) {
                Some(rest)
            } else if path.is_empty() || work.strip_prefix(*path).is_some_and(|rest| rest.starts_with('/')) {
                // A folder holding the worktree: all of it named.
                Some("")
            } else {
                None
            };
            rebased.extend(rel);
        }
        // The ignored folder (if any) is rebased with the paths: a proof made anew here would
        // drop it, and the walk under blaze's own folder would run every command.
        proof = Some(covered.rebased(work, &rebased));
    }
    // HEAD's tree is read only for a whole computation (git_objects.rs `status_after`).
    let tree = |files: &mut F, alloc: Alloc<'a>| head_tree(&repo, files, alloc);
    crate::git_objects::status_after(previous, proof.as_ref(), untracked, all, &repo.gitdir, &repo.common, work, tree, now, files, alloc, plant)
}

/// A repository: its own git folder (HEAD, index), the common one (refs, objects), and its
/// worktree (the folder holding `.git`), from the tree's root.
struct Repo<'a> {
    gitdir: AString<'a>,
    common: AString<'a>,
    work: AString<'a>,
}

/// The repository from `folder` up: a `.git` file (`gitdir: <path>`) or a `.git` folder with a
/// `HEAD`, the nearest first. Each place looked at is a read.
fn find<'a>(folder: &str, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<Repo<'a>, Refused> {
    let mut at = folder;
    let mut bytes = Vec::new_in(alloc);
    loop {
        let dot = join(at, ".git", alloc);
        bytes.clear();
        if files.read(&dot, &mut bytes) {
            let text = core::str::from_utf8(&bytes).map_err(|_| Refused::Malformed(GitFile::Gitdir))?;
            let path = text.strip_prefix("gitdir: ").ok_or(Refused::Malformed(GitFile::Gitdir))?.trim_end();
            let gitdir = locate(at, path, GitFile::Gitdir, files, alloc)?;
            let common = common_dir(&gitdir, files, alloc)?;
            return Ok(Repo { gitdir, common, work: String::from_str_in(at, alloc) });
        }
        bytes.clear();
        if files.read(&join(&dot, "HEAD", alloc), &mut bytes) {
            let common = common_dir(&dot, files, alloc)?;
            return Ok(Repo { gitdir: dot, common, work: String::from_str_in(at, alloc) });
        }
        if at.is_empty() {
            return Err(Refused::NoRepository);
        }
        at = at.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

/// The common folder of `gitdir`: its `commondir` file's, or itself.
fn common_dir<'a>(gitdir: &str, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<AString<'a>, Refused> {
    let mut bytes = Vec::new_in(alloc);
    if !files.read(&join(gitdir, "commondir", alloc), &mut bytes) {
        return Ok(String::from_str_in(gitdir, alloc));
    }
    let text = core::str::from_utf8(&bytes).map_err(|_| Refused::Malformed(GitFile::Commondir))?.trim_end();
    locate(gitdir, text, GitFile::Commondir, files, alloc)
}

/// `path` as a `.git` file in `base` names it (absolute, or relative to `base`), from the
/// tree's root, `.` and `..` resolved.
pub(crate) fn locate<'a>(base: &str, path: &str, file: GitFile, files: &impl GitFiles, alloc: Alloc<'a>) -> Result<AString<'a>, Refused> {
    if path.is_empty() {
        return Err(Refused::Malformed(file));
    }
    let joined = if path.starts_with('/') {
        String::from_str_in(files.within(path).ok_or(Refused::Outside(file))?, alloc)
    } else {
        join(base, path, alloc)
    };
    let mut parts: AVec<'_, &str> = Vec::new_in(alloc);
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop().ok_or(Refused::Outside(file))?;
            }
            part => parts.push(part),
        }
    }
    let mut out = String::new_in(alloc);
    for (at, part) in parts.iter().enumerate() {
        if at > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    Ok(out)
}

/// The commit the ref `name` names: its loose file (a symbolic one followed, at most five
/// deep), else its line of `packed-refs`.
fn resolve<'a>(repo: &Repo<'_>, name: &str, files: &mut impl GitFiles, alloc: Alloc<'a>, depth: u8) -> Result<AString<'a>, Refused> {
    if depth > 5 {
        return Err(Refused::Malformed(GitFile::LooseRef));
    }
    let mut bytes = Vec::new_in(alloc);
    if files.read(&join(&repo.common, name, alloc), &mut bytes) {
        let text = core::str::from_utf8(&bytes).map_err(|_| Refused::Malformed(GitFile::LooseRef))?.trim_end();
        if let Some(next) = text.strip_prefix("ref: ") {
            return resolve(repo, next, files, alloc, depth + 1);
        }
        if !is_commit(text) {
            return Err(Refused::Malformed(GitFile::LooseRef));
        }
        return Ok(String::from_str_in(text, alloc));
    }
    bytes.clear();
    if files.read(&join(&repo.common, "packed-refs", alloc), &mut bytes) {
        let text = core::str::from_utf8(&bytes).map_err(|_| Refused::Malformed(GitFile::PackedRefs))?;
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let Some((commit, named)) = line.split_once(' ') else { return Err(Refused::Malformed(GitFile::PackedRefs)) };
            if named == name {
                if !is_commit(commit) {
                    return Err(Refused::Malformed(GitFile::PackedRefs));
                }
                return Ok(String::from_str_in(commit, alloc));
            }
        }
    }
    Err(Refused::NoCommit)
}

/// A full object name: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
fn is_commit(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `base/name`, or `name` when `base` is the root.
fn join<'a>(base: &str, name: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::from_str_in(base, alloc);
    if !base.is_empty() {
        out.push('/');
    }
    out.push_str(name);
    out
}
