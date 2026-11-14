//! The no-op lane's core: when a build or a check of a checkout is answered as the checkout's
//! last one was, with no plan, no lowering, no task and no store ([`super::Jobs::begin`]
//! drives it; docs/cargo.md, "A build with nothing changed").
//!
//! An answer is kept ([`LastAnswer`]) only of a build that walked every unit and made
//! nothing ([`Recording::end`]), so what is said again is what a walk said of this very
//! state. A checkout keeps one a request shape ([`Answers`]: `build`, `test`, `run`, `check`,
//! each in a profile), so `build`, `test`, `build` finds the first `build`'s again: an answer
//! goes when a job of its own shape walks, never because a request of another shape did; what
//! that other walk changed that this answer rests on (an output of this answer's rewritten, a
//! lock file) moves the same stamps and roots that a stranger's change does.
//! It is said again only while every input of that walk is as it was
//! ([`LastAnswer::replayable`]), each input covered by one of:
//!
//! - **The checkout's files** (sources, manifests, the lock file, `.cargo/config.toml` in it,
//!   a build script's reads and listings in it, `include!`d files in it): the view's root
//!   ([`Inputs::view`]), which the checkout's watcher commits before the request is seen; or,
//!   where the root is another, each file that differs is a source the walk held by its code
//!   text alone, with the still text it had ([`LastAnswer::code_unmoved`], below).
//! - **The request**: its command, arguments, directory and the config files outside the
//!   view by their bytes, the mounts (each source and path dependency outside the view by its
//!   root), the toolchain, where the daemon builds, and the blaze that serves it
//!   ([`Inputs::config`]); and what the request says past those (`-v`, the build goal:
//!   [`Inputs::request`]).
//! - **The request's environment**, of which exactly the variables a step of the walk read
//!   ([`env_moved`]): those a step reads without asking for them by name ([`Asked::unasked`]:
//!   the plan's, the set a build script's walk of its environment sees, blaze's own), and
//!   those the walk's steps asked for by name ([`LastAnswer::named`]: a build script's traced
//!   `env::var`, a crate's `env!`). A variable nothing read (a shell's `SHLVL`, nushell's
//!   `CMD_DURATION_MS`, which is another at every command) is no input: the answer is said
//!   again whatever it holds. Until 2026-11-12 the whole environment was compared, "for every
//!   step of the build, a build script's reads among them, which no plan names": the reads
//!   are named now, by the steps that made them. A walk that cannot name them
//!   ([`Recording::vouch_env`] not told, or told why not) keeps an answer every variable is
//!   compared for. A `run` or `test` asked through the client is a build whose program the
//!   client starts in its own environment: no variable is an input of the answer for the
//!   program's sake.
//! - **How the request's client is shown diagnostics** (in colour or not, their width, a line
//!   each, how many refused functions are named: [`Inputs::drawn`]): no input of anything
//!   built, and of an answer only its diagnostics are drawn with it. An answer that holds no
//!   diagnostic is said again to a client shown them another way; one that holds any is not
//!   ([`Moved::Drawing`]), and the walk draws them for that client from the units' records
//!   (`serve::unit::drawn`), making nothing.
//! - **Paths outside every view** (a build script's traced reads and the directories it
//!   listed, an `include!`d file, the toolchain's files, the C compiler's): each look the walk
//!   made at the disk is a leaf ([`LastAnswer::leaves`]) with the stamp it had and the instant
//!   it was taken ([`Walk::looks`]), and each still holds ([`blaze_vfs::outside::holds`]: the
//!   same stamp now, and not taken within its change's tick, the racy rule). Nothing outside a
//!   checkout is watched: until 2026-11-14 a watch of each such directory vouched for it, and
//!   setting one up under `/root` walked 256,216 directories on the daemon's loop.
//! - **Paths outside the view that are mounted where they are** (a path dependency): each
//!   one's leaves hold since it was read ([`Asked::locals_vouched`], `Sweep::stale`);
//!   else the build reads it again, and walks.
//! - **The outputs**: each output file of the walk's plan, and what the units read off the
//!   disk beside them (the layout profiles' directory, there or [`Stamp::GONE`]), stamped
//!   before the walk looked at anything ([`LastAnswer::outputs`]); and each file under the
//!   outputs' directories a step read (a build script's `OUT_DIR`, [`Recording::vouch_reads`]),
//!   stamped as the walk ends. A removed, replaced or rewritten output moves its stamp; a test
//!   build's binary placed beside a build's is another file, which moves none of the build's
//!   stamps, so `build`, `test`, `build` answers the third.
//! - **Other jobs**: none is under way ([`Asked::alone`]).
//!
//! # The sweep, and its floor
//!
//! Asking a kept answer costs one `lstat` (a `stat` for a file looked at through its links)
//! per output and per leaf, and reads no file ([`LastAnswer::sweep`]): the floor of a check
//! that rests on stamps, so the ratio is 1.0. The sweep stops at the first stamp that does not
//! hold. It is the span `no-op answer: sweep the stamps`, with the counts of leaves and outputs
//! kept and how many were stamped; its time is the span's:
//!
//! ```sql
//! SELECT dur_ns, fields FROM spans WHERE name = 'no-op answer: sweep the stamps'
//! ```
//!
//! The lane reads no file again: a stamp that moved or is racy walks, and the walk reads.
//!
//! What is not covered refuses ([`NotReplayed`], [`Unkept`]): the build walks, which is the
//! whole build, and `blaze why` names the input ([`NotReplayed::input`]).
//!
//! # An edit that changes no code
//!
//! A unit is keyed by its files' code texts (`blaze_lower::code`), not their bytes, so an edit
//! of what no code text holds leaves every key as it was, and a walk after it finds every
//! record and says what the last one said. The lane answers such an edit too, at the file: no
//! job, no plan, no lowering. Exactly these edits qualify, and no other: in a Rust source the
//! walk held by code text alone ([`StillFile`]), a plain `//` comment's text, one that comes
//! or goes at a line's end (after code, or on a line that stays a line), the spaces and tabs
//! before a line's end, and what is after the file's last token (blank lines, `//` comment
//! lines). They are what `blaze_lower::still_text_of` leaves out; the daemon parses each
//! changed file once and hands the lane both still texts ([`FileEdit`]). An edit of a doc
//! comment, of a block comment, of a line that moves the lines after it, of any token, of a
//! file that is not such a source (a manifest, a file a build script or a macro reads, a
//! source of a package one unit of which is keyed by its bytes: clippy's, a crate that keeps
//! its whole text), or a file added or removed, refuses, and the build walks.
//!
//! A diagnostic quotes its file's lines, comments and all, so an answer whose diagnostics
//! name a changed file is not said again ([`NotReplayed::Quoted`]): the walk renders them
//! again from the places its record keeps (`serve::unit::compile`, `Placed`), which the lane
//! has not.
//!
//! A sans-IO core (rule 14), as `serve::why` is: it reads no file and no clock; the daemon
//! hands it each stamp it asks for ([`Stamps`]), the files that differ ([`Edited`]) and the
//! counts it read.

use std::{
    alloc::{Allocator, Global},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_api::{Response, Worked};
use blaze_core::Stamp;
use blaze_ids::{ContentId, Hasher};
use blaze_units::query::BuildConfig;
use blaze_vfs::outside::LookKind;

/// The step a refusal of the lane is under in `blaze why` ([`blaze_kernel::Miss::step`]).
pub(super) const STEP: &str = "no-op answer";

/// The stamp of a path now, as [`LastAnswer::sweep`] asks it: an `lstat` (a `stat` for a file
/// looked at through its links, [`LookKind::stamp_now`]) in the daemon, the model's disk in a
/// test.
pub(super) trait Stamps {
    fn stamp(&self, path: &Path, kind: LookKind) -> Stamp;
}

/// A path an answer rests on, with the stamp it had (`kind` says how it is taken again) and
/// the instant before that stamp was taken, by the file system's clock: what the racy rule
/// is asked of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Stamped {
    pub(super) path: PathBuf,
    pub(super) kind: LookKind,
    pub(super) stamp: Stamp,
    pub(super) looked_ns: u64,
}

impl Stamped {
    /// Why this does not hold given `now`, its stamp taken again; none when it holds. The
    /// rule [`blaze_vfs::outside::holds`] is, told apart for the reason.
    fn unheld(&self, now: &Stamp) -> Option<Unheld> {
        if now.is_unknown() || *now != self.stamp {
            return Some(Unheld::Moved);
        }
        if blaze_vfs::racy(&self.stamp, self.looked_ns) && !planted(Plant::TrustsRacy) {
            return Some(Unheld::Racy);
        }
        None
    }
}

/// Why a stamp kept with an answer does not hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Unheld {
    /// It is another now (written, made, removed, replaced, chmodded), or could not be taken.
    Moved,
    /// It was taken within its change's tick (the racy rule, [`blaze_vfs::racy`]): a write in
    /// the same tick after it would leave it as it was, so it vouches for nothing.
    Racy,
}

/// What a build's answer is a function of that the daemon holds by value.
#[derive(Clone, Debug)]
pub(super) struct Inputs {
    /// The view's root: every file of the checkout.
    pub(super) view: ContentId,
    /// The view's whole build configuration, the request's every variable in it.
    pub(super) config: Arc<BuildConfig>,
    /// The hash of `config`'s encoding less its environment, which is compared by what read
    /// it ([`env_moved`]).
    pub(super) config_id: ContentId,
    /// The hash of the request's work and its `Targets`, less how its client is shown
    /// diagnostics: what `config` does not hold of it (verbosity, the build goal).
    pub(super) request: ContentId,
    /// The hash of how the request's client is shown diagnostics: in colour or not, their
    /// width, a line each or whole, how many refused functions are named. Of an answer, only
    /// its diagnostics are a function of it.
    pub(super) drawn: ContentId,
}

impl Inputs {
    /// One id of all of them: what `blaze why` files a refusal under.
    pub(super) fn id(&self) -> ContentId {
        Hasher::new("blaze v2 serve no-op inputs")
            .id(&self.view)
            .id(&self.config_id)
            .id(&self.request)
            .id(&self.drawn)
            .finish()
    }

    /// The first input of `now` that is not as it is here, or none; of the environment, the
    /// variables `reads` says a step read ([`env_moved`]).
    pub(super) fn moved(&self, now: &Inputs, reads: &dyn Fn(&str) -> bool) -> Option<Moved> {
        if self.files_moved(now) {
            return Some(Moved::Files);
        }
        self.moved_past_files(now)
            .or_else(|| env_moved(&self.config, &now.config, reads).map(Moved::Env))
            .or(self.drawn_moved(now).then_some(Moved::Drawing))
    }

    /// Whether `now`'s client is shown diagnostics another way than this one's.
    fn drawn_moved(&self, now: &Inputs) -> bool {
        self.drawn != now.drawn && !planted(Plant::IgnoresRequest)
    }

    /// Whether `now`'s view is another root than this one's.
    fn files_moved(&self, now: &Inputs) -> bool {
        self.view != now.view && !planted(Plant::IgnoresFiles)
    }

    /// [`Inputs::moved`] of everything but the view, the environment and how diagnostics are
    /// drawn.
    fn moved_past_files(&self, now: &Inputs) -> Option<Moved> {
        if self.config_id != now.config_id && !planted(Plant::IgnoresConfig) {
            return Some(config_moved(&self.config, &now.config));
        }
        if self.request != now.request && !planted(Plant::IgnoresRequest) {
            return Some(Moved::Rendering);
        }
        None
    }
}

/// The first name of `was` whose value in `now` is another or none, else the first name of
/// `now` that `was` has not.
fn first_moved<V: PartialEq>(was: &[(String, V)], now: &[(String, V)]) -> Option<String> {
    let other = |list: &[(String, V)], name: &str| list.iter().position(|(it, _)| it == name);
    for (name, value) in was {
        match other(now, name) {
            Some(at) if now[at].1 == *value => {}
            _ => return Some(name.clone()), // clone: a reason, once a refusal
        }
    }
    // clone: a reason, once a refusal
    now.iter().find(|(name, _)| other(was, name).is_none()).map(|(name, _)| name.clone())
}

/// The first variable (by name, of `was` first) that `reads` says a step read and whose
/// value in `now`'s environment is not the one in `was`'s: set to another, unset, or set
/// where it was not. None whatever else of the two environments differs: a variable no step
/// read is no input of an answer (rule keys-cover-every-input, both ways).
pub(super) fn env_moved(
    was: &BuildConfig,
    now: &BuildConfig,
    reads: &dyn Fn(&str) -> bool,
) -> Option<String> {
    use blaze_units::query::env_var;
    if planted(Plant::IgnoresConfig) {
        return None;
    }
    let (was, now) = (&was.invocation.env, &now.invocation.env);
    let whole = planted(Plant::WholeEnvironment);
    let read = |name: &str| whole || reads(name);
    let moved = |it: &&(String, String)| {
        read(it.0.as_str()) && env_var(now, it.0.as_str()) != Some(it.1.as_str())
    };
    let added = |it: &&(String, String)| read(it.0.as_str()) && env_var(was, it.0.as_str()).is_none();
    was.iter().find(moved).or_else(|| now.iter().find(added)).map(|it| it.0.clone()) // clone: a reason, once a refusal
}

/// What of two configs that encode differently less their environments differs, the first
/// found in this order.
fn config_moved(was: &BuildConfig, now: &BuildConfig) -> Moved {
    let (a, b) = (&was.invocation, &now.invocation);
    if a.command != b.command {
        return Moved::Command;
    }
    if a.args != b.args {
        return Moved::Args;
    }
    if a.cwd != b.cwd || a.root != b.root {
        return Moved::Directory;
    }
    if let Some(path) = first_moved(&a.files, &b.files) {
        return Moved::ConfigFile(path);
    }
    let mount = was
        .mounts
        .iter()
        .find(|it| !now.mounts.contains(it))
        .or_else(|| now.mounts.iter().find(|it| !was.mounts.contains(it)));
    if let Some(mount) = mount {
        return Moved::Mount(mount.at.clone()); // clone: a reason, once a refusal
    }
    if was.host != now.host || was.library != now.library {
        return Moved::Toolchain;
    }
    if was.blaze != now.blaze {
        return Moved::Blaze;
    }
    Moved::Places
}

/// An input of a kept answer that a request has another value of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Moved {
    /// A file of the checkout: the view's root is another, and which files differ was not
    /// read, or the answer's walk vouched for no source.
    Files,
    /// This file of the checkout, by its path from the view's root: it differs, and is not a
    /// source the answer's walk held by code text alone with the still text it had.
    File(String),
    Command,
    Args,
    /// The directory the command runs in, or the checkout's root.
    Directory,
    /// A variable of the request's environment that a step of the kept answer's walk read,
    /// by name; never its values.
    Env(String),
    /// A config file outside the view (`$CARGO_HOME/config.toml`, one above the checkout).
    ConfigFile(String),
    /// What is mounted at this path: a fetched source, or a path dependency outside the view.
    Mount(String),
    Toolchain,
    /// The version of the blaze that serves the build.
    Blaze,
    /// Where the daemon places builds, sources or the standard library's builds.
    Places,
    /// What the request says past its config: verbosity, the build goal.
    Rendering,
    /// How the request's client is shown diagnostics (colour, width, a line each, how many
    /// refused functions are named), where the kept answer holds diagnostics, which were
    /// drawn for another.
    Drawing,
}

impl fmt::Display for Moved {
    /// The input as a noun, what `blaze why` says changed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Moved::Files => f.write_str("a file of the checkout"),
            Moved::File(path) => write!(f, "the checkout's file {path}"),
            Moved::Command => f.write_str("the command"),
            Moved::Args => f.write_str("the command line's arguments"),
            Moved::Directory => f.write_str("the directory the command runs in"),
            Moved::Env(name) => write!(f, "the request's variable {name}"),
            Moved::ConfigFile(path) => write!(f, "the config file {path}"),
            Moved::Mount(at) => write!(f, "the source mounted at {at}"),
            Moved::Toolchain => f.write_str("the toolchain"),
            Moved::Blaze => f.write_str("the version of blaze"),
            Moved::Places => f.write_str("where the daemon places builds and sources"),
            Moved::Rendering => {
                f.write_str("what the request says past its command line (`-v`, `--goal`)")
            }
            Moved::Drawing => f.write_str(
                "how the last build's diagnostics were drawn (colors, width, `--message-format`, `--refused`)",
            ),
        }
    }
}

/// Why a checkout has no kept answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Unkept {
    /// No build of it has ended in this daemon, or a job of it walks now.
    None,
    /// Its last build did not pass.
    Failed,
    /// Its last build compiled, placed, missed or shared something: the next one walks, and
    /// that one's answer is kept.
    Made,
    /// Its last build planned anew, or its plan wrote a lock file: the directories of its
    /// outputs were not known before it looked at anything.
    Planned,
    /// Its last build said what a build with nothing to do does not say.
    Said(&'static str),
    /// Its last build looked at this path outside every view on the disk, and the look has no
    /// stamp to be swept by (its look kept none, or it said two things).
    Looked(PathBuf),
    /// Its last build kept no list of its looks at the disk ([`blaze_vfs::outside::Looks`]),
    /// so nothing says what it looked at.
    Unlooked,
    /// This output of its last build, or this directory its outputs are in, is one no stamp
    /// can vouch for: a directory that is not there or is named through a symlink.
    Outputs(PathBuf),
    /// Its builds read this off the disk where the daemon counts no look and keeps no stamp.
    Uncounted(&'static str),
    /// Its steps did not say which files under its outputs' directories they read
    /// ([`Recording::vouch_reads`]), and why: nothing would sweep them.
    Unread(String),
}

impl Unkept {
    /// The input no stamp or key covers, as a noun: what `blaze why` names. None for a build
    /// whose answer is not kept for what it did, which the next build's is.
    fn input(&self) -> Option<String> {
        match self {
            Unkept::None | Unkept::Failed | Unkept::Made | Unkept::Planned => None,
            // alloc: a reason, once a refusal
            Unkept::Said(what) => Some(format!("what the last build said ({what})")),
            Unkept::Looked(path) => Some(format!(
                "{}, which the last build looked at with no stamp kept, so each build looks at it on the disk",
                path.display()
            )),
            Unkept::Unlooked => {
                Some("what the last build looked at on the disk, of which it kept no list".to_owned())
            }
            Unkept::Outputs(path) => Some(format!(
                "{}, which no stamp can vouch for (a directory of outputs that is not there or is named through a symlink)",
                path.display()
            )),
            Unkept::Uncounted(what) => {
                Some(format!("{what}, which each build reads off the disk and no stamp covers"))
            }
            Unkept::Unread(why) => Some(format!(
                "the files under the outputs' directories the last build's steps read, which they did not say ({why})"
            )),
        }
    }
}

impl fmt::Display for Unkept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unkept::None => f.write_str("no build of it that made nothing has ended"),
            Unkept::Failed => f.write_str("its last build did not pass"),
            Unkept::Made => {
                f.write_str("its last build compiled, placed, missed or shared something")
            }
            Unkept::Planned => {
                f.write_str("its last build planned anew or wrote a lock file")
            }
            Unkept::Said(what) => write!(f, "its last build said {what}"),
            Unkept::Looked(path) => write!(
                f,
                "its last build looked at {} on the disk and kept no stamp of it",
                path.display()
            ),
            Unkept::Unlooked => f.write_str("its last build kept no list of what it looked at on the disk"),
            Unkept::Outputs(path) => write!(
                f,
                "{} of its last build's outputs is one no stamp can vouch for",
                path.display()
            ),
            Unkept::Uncounted(what) => {
                write!(f, "its builds read {what} off the disk, which no stamp covers")
            }
            Unkept::Unread(why) => write!(
                f,
                "its steps did not say which files under its outputs' directories they read: {why}"
            ),
        }
    }
}

/// Why a checkout's last answer is not said again, and the build walks its units.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum NotReplayed {
    /// None is kept.
    NoAnswer(Unkept),
    /// The request asks for its trace, which is of a build that walks.
    Traced,
    /// An input the answer is keyed by is another.
    Moved(Moved),
    /// Another job is under way, which may be writing the outputs now.
    NotAlone,
    /// A path outside the view that is mounted where it is (a path dependency) has a leaf
    /// whose stamp no longer holds since it was read, or none: the build reads it again.
    Locals,
    /// The stamp of this output of the answer's walk, or of a path its outputs rest on, does
    /// not hold ([`Unheld`]).
    Output(PathBuf, Unheld),
    /// The stamp of this path outside every view, which a step of the answer's walk read, does
    /// not hold ([`Unheld`]).
    Leaf(PathBuf, Unheld),
    /// This file of the checkout changed in what no code text holds, and a diagnostic of the
    /// answer names it: its quoted lines may say otherwise now, and only a walk renders
    /// them again.
    Quoted(String),
}

impl NotReplayed {
    /// The input that moved or that nothing covers, as a noun: what `blaze why` says of a
    /// build that walked and made nothing. None where the build's own work says why it
    /// walked (none kept yet, or the last build made something).
    pub(super) fn input(&self) -> Option<String> {
        match self {
            NotReplayed::NoAnswer(unkept) => unkept.input(),
            // alloc: a reason, once a refusal
            NotReplayed::Traced => Some("the request's variable BLAZE_PROFILE".to_owned()),
            NotReplayed::Moved(moved) => Some(moved.to_string()),
            NotReplayed::NotAlone => Some("the jobs under way".to_owned()),
            NotReplayed::Locals => {
                Some("a path dependency outside the checkout, whose stamps moved".to_owned())
            }
            NotReplayed::Output(path, _) => Some(format!("the output {}", path.display())),
            NotReplayed::Leaf(path, _) => Some(path.display().to_string()),
            NotReplayed::Quoted(path) => {
                Some(format!("the lines of {path} that the last build's diagnostics quote"))
            }
        }
    }
}

impl fmt::Display for NotReplayed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotReplayed::NoAnswer(unkept) => write!(f, "no answer is kept: {unkept}"),
            NotReplayed::Traced => f.write_str("the request asks for its trace"),
            NotReplayed::Moved(moved) => write!(f, "{moved} is another than the kept answer's"),
            NotReplayed::NotAlone => f.write_str("another job is under way"),
            NotReplayed::Locals => f.write_str(
                "a path outside the view that is mounted where it is has a stamp that moved since it was read",
            ),
            NotReplayed::Output(path, Unheld::Moved) => write!(
                f,
                "the output {} is not as its stamp said when the kept build began",
                path.display()
            ),
            NotReplayed::Leaf(path, Unheld::Moved) => write!(
                f,
                "{}, outside the view, is not as its stamp said when the kept build looked at it",
                path.display()
            ),
            NotReplayed::Output(path, Unheld::Racy) | NotReplayed::Leaf(path, Unheld::Racy) => write!(
                f,
                "the stamp of {} was taken within its change's tick, so by the racy rule it vouches for nothing",
                path.display()
            ),
            NotReplayed::Quoted(path) => write!(
                f,
                "{path} changed in what no code reads, and a diagnostic of the kept answer quotes it"
            ),
        }
    }
}

/// A bug a test plants in the lane, which its oracle has to catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    /// Says an answer again whatever happened to the outputs: a replaced output keeps the
    /// answer (a deleted binary, another's write over it, `blaze clean`, layout profiles made).
    IgnoresOutputs,
    /// A leaf whose stamp moved is replayed: says an answer again whatever happened outside
    /// the view (a file a build script traced, a file added to a directory it listed).
    IgnoresLeaves,
    /// A racy stamp is trusted: one taken within its change's tick holds while it is the
    /// same, though a write later in that tick left it so.
    TrustsRacy,
    /// Says an answer of another view again (a source edit, the lock file).
    IgnoresFiles,
    /// Says an answer of another config again (a variable, a flag, a feature, the profile).
    IgnoresConfig,
    /// Says an answer of another request again, and one whose diagnostics were drawn for
    /// another client (colors, width).
    IgnoresRequest,
    /// Says an answer again while another job may be writing the outputs.
    IgnoresOthers,
    /// Says an answer again though a path dependency outside the view is to be read again.
    IgnoresLocals,
    /// Keeps the answer of a walk with a look at the disk that kept no stamp.
    KeepsALook,
    /// Keeps the answer of a build that made something.
    KeepsWork,
    /// Keyed by code text, and takes a source whose tokens changed for one whose comments
    /// did: says an answer again over a body edit.
    IgnoresTokens,
    /// Takes every file whose still text is as it was for a source held by code text: says
    /// an answer again over a comment edit in a file a build script, a lint or a macro reads
    /// by its bytes.
    IgnoresHeld,
    /// Says an answer again though a diagnostic of it quotes a file that changed: a quoted
    /// line with a comment the file no longer has.
    ReplaysQuotes,
    /// Compares the request's whole environment, as the lane did until 2026-11-12: a
    /// variable nothing read turns a build with nothing changed into a walk.
    WholeEnvironment,
    /// Compares no variable the walk's steps asked for by name: says an answer again over a
    /// variable a build script read.
    IgnoresNamed,
    /// Drops every shape's answer when a job of one shape walks, as the lane did until
    /// 2026-11-12 (one answer a checkout): `build`, `test`, `build` walks three times.
    DropsOthers,
}

#[cfg(test)]
thread_local! {
    /// The bug the test on this thread planted.
    pub(super) static PLANTED: std::cell::Cell<Option<Plant>> = const { std::cell::Cell::new(None) };
}

/// Whether the test on this thread planted `plant`; never outside a test.
pub(super) fn planted(plant: Plant) -> bool {
    #[cfg(test)]
    let planted = PLANTED.get() == Some(plant);
    #[cfg(not(test))]
    let planted = {
        _ = plant;
        false
    };
    planted
}

/// A Rust source of the checkout that every key of a walk held by its code text alone
/// (`blaze_lower::Lowered::still_sources`, less what a build script read): its path from the
/// view's root, and the edition its crates parse it in, which the daemon parses it in to
/// work out its still text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct StillFile {
    pub(super) path: String,
    pub(super) edition: ::span::Edition,
}

/// A file that differs between a kept answer's view and the view asked, as the daemon read
/// it: its path from the view's root, and, when it is there in both as text, the still text
/// (`blaze_lower::still_text`) of each, the answer's first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct FileEdit {
    pub(super) path: String,
    pub(super) still: Option<(ContentId, ContentId)>,
}

/// How the files of the view asked differ from those of a kept answer's view.
#[derive(Clone, Copy, Debug)]
pub(super) enum Edited<'a> {
    /// Not read, and why: the answer's view is no longer held, more files differ than the
    /// lane reads ([`MOST_EDITS`]), or the roots are one.
    Unread(&'static str),
    /// Each file that differs, by path.
    Files(&'a [FileEdit]),
}

/// The most files that may differ for the lane to read them: past it the build walks. Each
/// costs the daemon's loop one parse of the file as it is now (and one of it as it was, the
/// first time), so the bound is what keeps the lane's answer in milliseconds.
pub(super) const MOST_EDITS: usize = 8;
/// The most bytes of one changed file the lane parses: a longer one walks.
pub(super) const MOST_BYTES: usize = 1 << 20;

/// A request, as the lane is asked of it.
pub(super) struct Asked<'a> {
    pub(super) inputs: &'a Inputs,
    /// How its view's files differ from the kept answer's view's, where the roots differ.
    pub(super) edited: Edited<'a>,
    /// Its environment asks for the build's trace (`BLAZE_PROFILE`).
    pub(super) traced: bool,
    /// No other job is under way, nor a store of the checkout's kept front end.
    pub(super) alone: bool,
    /// Every path outside the view that is mounted where it is on the disk has leaves whose
    /// stamps hold since it was read (`Sweep::stale` names none).
    pub(super) locals_vouched: bool,
    /// Whether a step reads the variable of this name without asking for it by name: the
    /// plan's (`blaze_units::query::read_by_plan`), what a build script's walk of its
    /// environment sees (`blaze_usercode::confine::walk_sees`), blaze's own (`BLAZE_*`).
    /// The lists are the steps' own, handed in by the daemon: a core reads none.
    pub(super) unasked: &'a dyn Fn(&str) -> bool,
}

/// The answer of a checkout's last build that walked every unit and made nothing: what the
/// next build of the same inputs says again without walking, while nothing it rests on moved
/// ([`LastAnswer::replayable`]). Only such a build's answer is kept, never one that compiled,
/// placed or missed anything, so what is said again is what a walk said of this very state:
/// its warnings, its units and its artifacts as a build with nothing to do says them.
///
/// Kept in memory, one a request shape of a checkout ([`Answers`]), and dropped by the next
/// job of that shape that walks; a restarted daemon keeps none, and walks.
pub(super) struct LastAnswer {
    pub(super) inputs: Inputs,
    /// Its outputs and what they rest on (the layout profiles' directory, there or not),
    /// stamped before the build looked at anything ([`super::recorded_dirs`]), and each file
    /// under the outputs' directories its steps read, stamped as it ended
    /// ([`Recording::vouch_reads`]).
    pub(super) outputs: Arc<[Stamped]>,
    /// Each look its walk made at the disk outside every view, with the stamp the look took
    /// ([`Walk::looks`]).
    pub(super) leaves: Arc<[Stamped]>,
    /// What it said, in order, but its `Work` and its progress.
    pub(super) responses: Arc<[Response]>,
    /// Its `Work`, for a build; a check says none.
    pub(super) worked: Option<Worked>,
    /// The sources its walk held by code text alone, sorted by path; or why it vouched for
    /// none ([`Recording::vouch`]).
    pub(super) still: Result<Arc<[StillFile]>, String>,
    /// The last view asked that differs from `inputs.view` only in what no code text holds
    /// ([`LastAnswer::code_unmoved`] said so): asked again, it is not read again.
    pub(super) same: Option<ContentId>,
    /// The variables of the request's environment its walk's steps asked for by name, sorted;
    /// or why they are not known ([`Recording::vouch_env`]), and then every variable is one a
    /// step may have read.
    pub(super) named: Result<Arc<[String]>, String>,
}

/// Whether `diagnostic`, or one under it, names a file whose last name is `name`: in a span,
/// or in its rendered text (every snippet is under its file's path there).
fn names_file(diagnostic: &blaze_api::Diagnostic, name: &str) -> bool {
    diagnostic.rendered.contains(name)
        || diagnostic.spans.iter().any(|span| span.file.ends_with(name))
        || diagnostic.children.iter().any(|child| names_file(child, name))
}

impl LastAnswer {
    /// Whether a step of the answer's walk read the variable `name`: one a step reads unasked
    /// (`unasked`), or one its steps asked for by name; any, where those are not known.
    fn read_env(&self, name: &str, unasked: &dyn Fn(&str) -> bool) -> bool {
        if unasked(name) {
            return true;
        }
        match &self.named {
            Ok(names) => {
                names.binary_search_by(|it| it.as_str().cmp(name)).is_ok()
                    && !planted(Plant::IgnoresNamed)
            }
            Err(_) => true,
        }
    }

    /// About what keeping the answer holds of the heap, in bytes: what it said (each
    /// response's text), the paths it rests on with their stamps, the sources it vouched for
    /// by path and the names its steps asked for. Its config is shared with nothing after
    /// its walk, and is counted by its environment and arguments. Not counted: the view of
    /// its files the daemon holds open beside it (the nodes of the tree no later root
    /// shares). A count for a budget, not an exact size.
    pub(super) fn held_bytes(&self) -> usize {
        let text = |response: &Response| match response {
            Response::Diagnostic(diagnostic) => diagnostic.rendered.len() + diagnostic.message.len(),
            _ => 0,
        };
        let said: usize =
            self.responses.iter().map(|it| size_of::<Response>() + text(it)).sum();
        let paths = |all: &[Stamped]| -> usize {
            all.iter().map(|it| size_of::<Stamped>() + it.path.as_os_str().len()).sum()
        };
        let still: usize = match &self.still {
            Ok(files) => files.iter().map(|it| size_of::<StillFile>() + it.path.len()).sum(),
            Err(why) => why.len(),
        };
        let named: usize = match &self.named {
            Ok(names) => names.iter().map(|it| size_of::<String>() + it.len()).sum(),
            Err(why) => why.len(),
        };
        let invocation = &self.inputs.config.invocation;
        let env: usize = invocation.env.iter().map(|(name, value)| name.len() + value.len()).sum();
        let args: usize = invocation.args.iter().map(String::len).sum();
        size_of::<LastAnswer>() + said + paths(&self.outputs) + paths(&self.leaves) + still + named + env + args
    }

    /// Whether `view` is known to hold the answer's files as every key of its walk read
    /// them: its own view, or the last one found so.
    pub(super) fn files_known(&self, view: &ContentId) -> bool {
        self.inputs.view == *view || self.same == Some(*view)
    }

    /// Whether a diagnostic of the answer names the file at `path`, by its last name: a
    /// diagnostic names a file as its crate was passed it, which is not always its path from
    /// the view's root, so any file of that name counts.
    fn quotes(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        self.responses.iter().any(|response| match response {
            Response::Diagnostic(diagnostic) => names_file(diagnostic, name),
            _ => false,
        })
    }

    /// That `asked`'s view, another root than the answer's, holds no file a key of the
    /// answer's walk reads otherwise: each file that differs is a source the walk held by
    /// code text alone ([`LastAnswer::still`]), there in both views with one still text, and
    /// no diagnostic of the answer names it. The first that is not is the reason.
    fn code_unmoved(&self, asked: &Asked<'_>) -> Result<(), NotReplayed> {
        if self.same == Some(asked.inputs.view) {
            return Ok(());
        }
        let files = NotReplayed::Moved(Moved::Files);
        let (Ok(still), Edited::Files(edits)) = (&self.still, asked.edited) else {
            return Err(files);
        };
        // Roots that differ in no file differ in a mode or a link: not read here.
        if edits.is_empty() || edits.len() > MOST_EDITS {
            return Err(files);
        }
        for edit in edits {
            let held = still.binary_search_by(|it| it.path.as_str().cmp(edit.path.as_str())).is_ok()
                || planted(Plant::IgnoresHeld);
            let same = match edit.still {
                Some((was, now)) => was == now || planted(Plant::IgnoresTokens),
                None => false,
            };
            if !held || !same {
                return Err(NotReplayed::Moved(Moved::File(edit.path.clone()))); // clone: a reason, once
            }
            if self.quotes(&edit.path) && !planted(Plant::ReplaysQuotes) {
                return Err(NotReplayed::Quoted(edit.path.clone())); // clone: a reason, once
            }
        }
        Ok(())
    }

    /// Whether `asked` is answered by this: every input of the answer's walk is as it was
    /// (the module's list). The first that is not is the reason.
    pub(super) fn replayable(
        &self,
        asked: &Asked<'_>,
        disk: &impl Stamps,
    ) -> Result<(), NotReplayed> {
        if asked.traced {
            return Err(NotReplayed::Traced);
        }
        if self.inputs.files_moved(asked.inputs) {
            self.code_unmoved(asked)?;
        }
        if let Some(moved) = self.inputs.moved_past_files(asked.inputs) {
            return Err(NotReplayed::Moved(moved));
        }
        // Of the environment, what a step of this answer's walk read, and no other variable.
        let reads = |name: &str| self.read_env(name, asked.unasked);
        if let Some(name) = env_moved(&self.inputs.config, &asked.inputs.config, &reads) {
            return Err(NotReplayed::Moved(Moved::Env(name)));
        }
        // Only a diagnostic is drawn for its client: an answer with none is every client's.
        if self.inputs.drawn_moved(asked.inputs)
            && self.responses.iter().any(|it| matches!(it, Response::Diagnostic(_)))
        {
            return Err(NotReplayed::Moved(Moved::Drawing));
        }
        if !asked.alone && !planted(Plant::IgnoresOthers) {
            return Err(NotReplayed::NotAlone);
        }
        if !asked.locals_vouched && !planted(Plant::IgnoresLocals) {
            return Err(NotReplayed::Locals);
        }
        self.sweep(disk)
    }

    /// The sweep: each output's stamp, then each leaf's, taken again of `disk` and held to
    /// the one rule ([`Stamped::unheld`]); the first that does not hold is the reason. One
    /// stamp a path, no read: the floor (module doc). In a span with what it swept.
    pub(super) fn sweep(&self, disk: &impl Stamps) -> Result<(), NotReplayed> {
        let span = tracing::info_span!(
            target: "blaze",
            "no-op answer: sweep the stamps",
            outputs = self.outputs.len(),
            leaves = self.leaves.len(),
            stamped = tracing::field::Empty,
            reread = 0u64,
        )
        .entered();
        let mut stamped = 0u64;
        let mut first = |rows: &[Stamped], skip: bool| -> Option<(PathBuf, Unheld)> {
            if skip {
                return None;
            }
            rows.iter().find_map(|row| {
                stamped += 1;
                // clone: a reason, once
                row.unheld(&disk.stamp(&row.path, row.kind)).map(|why| (row.path.clone(), why))
            })
        };
        let outputs = first(&self.outputs, planted(Plant::IgnoresOutputs));
        let found = match outputs {
            Some((path, why)) => Err(NotReplayed::Output(path, why)),
            None => match first(&self.leaves, planted(Plant::IgnoresLeaves)) {
                Some((path, why)) => Err(NotReplayed::Leaf(path, why)),
                None => Ok(()),
            },
        };
        span.record("stamped", stamped);
        found
    }
}

/// A checkout's kept answer, or why it has none.
pub(super) type Last = Result<LastAnswer, Unkept>;

/// `view` noted as one `last`'s answer was said again of ([`LastAnswer::same`]), once
/// [`ask`] answered it.
pub(super) fn answered(last: &mut Last, view: ContentId) {
    if let Ok(answer) = last
        && answer.inputs.view != view
    {
        answer.same = Some(view);
    }
}

/// What `asked` is answered with from `last`: the kept answer's responses and its `Work`,
/// or why the build walks.
pub(super) fn ask(
    last: &Last,
    asked: &Asked<'_>,
    disk: &impl Stamps,
) -> Result<(Arc<[Response]>, Option<Worked>), NotReplayed> {
    match last {
        Err(unkept) => Err(NotReplayed::NoAnswer(unkept.clone())), // clone: a reason, once
        Ok(answer) => answer.replayable(asked, disk).map(|()| {
            // clone: a refcount and the `Work`, once an answer said again
            (Arc::clone(&answer.responses), answer.worked.clone())
        }),
    }
}

/// How a recorded build ended.
pub(super) struct Walk {
    /// It passed, with no error.
    pub(super) passed: bool,
    /// It compiled, missed, placed and shared nothing, and was not cancelled.
    pub(super) made_nothing: bool,
    /// Every look of the walk at the disk outside every view, with what each saw and the
    /// stamp it took: the build's own [`blaze_vfs::outside::Looks`] as it ended
    /// ([`Looks::all`]). When each kept a stamp and said one thing, the answer is kept with
    /// them as leaves, swept as the lane is asked; a look with no stamp keeps no answer.
    ///
    /// [`Looks::all`]: blaze_vfs::outside::Looks::all
    pub(super) looks: Vec<blaze_vfs::outside::Look>,
    /// Its `Work`.
    pub(super) worked: Option<Worked>,
}

/// A build that may become its checkout's [`LastAnswer`]: one that walks the checkout's last
/// plan. What it says is noted as the daemon tells it ([`Recording::hear`]), and it is kept
/// once it ends, if it made nothing ([`Recording::end`]).
pub(super) struct Recording {
    inputs: Inputs,
    /// The instant before it looked at anything, by the file system's clock: what a stamp
    /// taken as it ends is held to by the racy rule, so a write while it ran never holds.
    looked_ns: u64,
    /// The directories its outputs are in: a file a step read under one of them is swept.
    dirs: Vec<PathBuf>,
    /// [`LastAnswer::outputs`] so far.
    outputs: Vec<Stamped>,
    responses: Vec<Response>,
    /// Why it is not kept, once something says so.
    refused: Option<Unkept>,
    /// [`LastAnswer::still`], as its front end vouched ([`Recording::vouch`]).
    still: Result<Arc<[StillFile]>, String>,
    /// [`LastAnswer::named`], as its steps said ([`Recording::vouch_env`]).
    named: Result<Arc<[String]>, String>,
    /// Whether its steps said which files under its outputs' directories they read
    /// ([`Recording::vouch_reads`]); else why not, and no answer is kept.
    read: Result<(), String>,
}

impl Recording {
    /// A recording of a build of `inputs` whose outputs are in `dirs`, with `outputs` (its
    /// output files and what they rest on) stamped by the daemon at `looked_ns`, before it
    /// looks at anything. Its looks are its build's own ([`Walk::looks`]), so none is counted
    /// here.
    pub(super) fn new(
        inputs: Inputs,
        looked_ns: u64,
        dirs: Vec<PathBuf>,
        outputs: impl IntoIterator<Item = Stamped>,
    ) -> Recording {
        let mut kept = Vec::new_in(Global);
        kept.extend(outputs);
        Recording {
            inputs,
            looked_ns,
            dirs,
            outputs: kept,
            responses: Vec::new(), // alloc: once a build that walks its checkout's last plan
            refused: None,
            // alloc: a reason, once a build that walks its checkout's last plan
            still: Err("its front end was not asked which sources it held by code text".to_owned()),
            // alloc: as above
            named: Err("its steps were not asked which variables they read by name".to_owned()),
            // alloc: as above
            read: Err("its steps were not asked which paths they read".to_owned()),
        }
    }

    /// The paths the build's steps read by path, as they say once every task of the build
    /// ended: each file (`files`) and each directory listed or hashed whole (`listed`); or
    /// why they are not all known, and then no answer is kept. Those under a directory of
    /// its outputs (a build script's `OUT_DIR`) join [`LastAnswer::outputs`], each stamped
    /// now of `disk` and held to the recording's `looked_ns`, so one written while the build
    /// ran is racy and never holds. The rest are in a view, or looks ([`Walk::looks`]).
    pub(super) fn vouch_reads(
        &mut self,
        read: Result<(&[PathBuf], &[PathBuf]), &str>,
        disk: &impl Stamps,
    ) {
        let (files, listed) = match read {
            Ok(read) => read,
            Err(why) => {
                self.read = Err(why.to_owned());
                return;
            }
        };
        let under = |path: &Path| self.dirs.iter().any(|dir| path.starts_with(dir));
        // A listing of a directory above one of them is swept too: an entry made there moves it.
        let above = |path: &Path| self.dirs.iter().any(|dir| dir.starts_with(path));
        let mut swept = Vec::new_in(Global);
        swept.extend(files.iter().filter(|it| under(it)).map(|path| (path.as_path(), LookKind::Entry)));
        swept.extend(
            listed.iter().filter(|it| under(it) || above(it)).map(|path| (path.as_path(), LookKind::Listing)),
        );
        swept.sort_unstable();
        swept.dedup();
        let looked_ns = self.looked_ns;
        self.outputs.extend(swept.into_iter().map(|(path, kind)| Stamped {
            stamp: disk.stamp(path, kind),
            path: path.to_owned(), // clone: kept with the answer, once a build kept
            kind,
            looked_ns,
        }));
        self.read = Ok(());
    }

    /// The variables of the request's environment the build's steps asked for by name, as
    /// they say once every task of the build ended (each build script's kept run's traced
    /// reads, each unit record's `env!` and `option_env!`); or why they are not all known.
    /// A recording not told keeps an answer every variable is compared for.
    pub(super) fn vouch_env(&mut self, named: Result<Vec<String>, String>) {
        self.named = named.map(|mut names| {
            names.sort_unstable();
            names.dedup();
            names.into()
        });
    }

    /// The sources the build's front end held by code text alone, as it says once every
    /// task of the build ended, sorted by path; or why it vouches for none. A recording not
    /// told keeps an answer no changed file is said again over.
    pub(super) fn vouch(&mut self, still: Result<Vec<StillFile>, String>) {
        self.still = still.map(|mut files| {
            files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
            files.dedup_by(|a, b| a.path == b.path);
            files.into()
        });
    }

    /// A response the build said: kept in order when it is one a build with nothing to do
    /// says; its `Work` and its progress are made anew; anything else leaves it unkept.
    pub(super) fn hear(&mut self, response: &Response) {
        if self.refused.is_some() {
            return;
        }
        match response {
            Response::Work(_) | Response::Doing(_) => {}
            Response::Diagnostic(_)
            | Response::Unit(_)
            | Response::UnitBegan(_)
            | Response::UnitEnded(_)
            | Response::BuildScript(_)
            | Response::Built { .. } => self.responses.push(response.clone()), // clone: kept to be said again
            _ => {
                self.refused = Some(Unkept::Said("what a build with nothing to do does not say"));
                // What it said so far is of no use: let go now, not at its end.
                self.responses = Vec::new(); // alloc: none
            }
        }
    }

    /// The build ended as `walk` says: its answer, kept when it passed, made nothing, said
    /// only what such a build says, its steps said what they read under its outputs, and
    /// each look it made at the disk kept a stamp and said one thing (each is a leaf
    /// [`LastAnswer::sweep`] takes again). Else why it is not kept.
    pub(super) fn end(self, walk: Walk) -> Last {
        if !walk.passed {
            return Err(Unkept::Failed);
        }
        if let Some(why) = self.refused {
            return Err(why);
        }
        if !walk.made_nothing && !planted(Plant::KeepsWork) {
            return Err(Unkept::Made);
        }
        if let Err(why) = self.read {
            return Err(Unkept::Unread(why));
        }
        // The invariant: an answer is said again only while everything its walk rested on
        // is as it was. Each look the walk made at the disk is a leaf with the stamp it took,
        // swept as the lane is asked; a look that kept no stamp keeps no answer. Until
        // 2026-11-14 a watch vouched for each directory outside every view, and a look of
        // other than a file kept none: a build script that listed a directory was a walk in
        // every build.
        let leaves = match leaves_of(&walk.looks, Global) {
            Ok(leaves) => leaves,
            Err(_) if planted(Plant::KeepsALook) => Vec::new_in(Global),
            Err(at) => return Err(Unkept::Looked(at)),
        };
        Ok(LastAnswer {
            inputs: self.inputs,
            outputs: self.outputs.into(),
            leaves: leaves.into(),
            responses: self.responses.into(),
            worked: walk.worked,
            still: self.still,
            same: None,
            named: self.named,
        })
    }
}

/// The looks of a walk as leaves, when every one kept a stamp and saw one thing
/// ([`blaze_vfs::outside::Looks::twice`] finds none); else the path of the first look that is
/// not so, for the reason.
fn leaves_of<A: Allocator + Clone>(looks: &[blaze_vfs::outside::Look], alloc: A) -> Result<Vec<Stamped, A>, PathBuf> {
    if let Some((first, _)) = blaze_vfs::outside::Looks::twice(looks).next() {
        return Err(first.path.clone()); // clone: a reason, once
    }
    let mut leaves = Vec::with_capacity_in(looks.len(), alloc);
    for look in looks {
        if look.stamp.is_unknown() {
            return Err(look.path.clone()); // clone: a reason, once
        }
        leaves.push(Stamped {
            path: look.path.clone(), // clone: kept with the answer, once a build kept
            kind: look.kind,
            stamp: look.stamp,
            looked_ns: look.looked_ns,
        });
    }
    Ok(leaves)
}

/// The most request shapes a checkout keeps an answer of ([`Answers`]): the plans it keeps
/// (`PLANS_KEPT` in the daemon, which asserts the two are one), a shape being what a plan is
/// kept by less its view. A count now; what one costs to keep is [`LastAnswer::held_bytes`]
/// and the view held beside it: a footprint budget lets go of the least recently used first
/// (the first of [`Answers`]' shapes).
pub(super) const MOST_SHAPES: usize = 8;

/// One request shape's place among a checkout's answers.
struct Shaped<H> {
    shape: ContentId,
    last: Last,
    /// What the daemon holds beside the answer for as long as it is kept (the view of the
    /// files its walk read): handed back when the answer goes.
    held: Option<H>,
}

/// A checkout's kept answers, one a request shape (the request's config as its plan reads
/// it: the command, the arguments, the plan's variables), the most recently used last, at
/// most [`MOST_SHAPES`]. The store is the lane's, so its rule is simulated with it:
///
/// - a job of a shape that walks drops that shape's answer ([`Answers::begin`]) and may
///   leave the next ([`Answers::keep`]);
/// - nothing else drops an answer but the bound, which lets go of the least recently used;
/// - a request is asked of its own shape's answer alone ([`Answers::ask`]), which is said
///   again only while every input of its walk is as it was ([`LastAnswer::replayable`]).
///
/// So a walk of another shape costs an answer nothing unless it changed something the
/// answer rests on, which the answer's own check then finds.
pub(super) struct Answers<H> {
    shapes: Vec<Shaped<H>>,
}

impl<H> Answers<H> {
    pub(super) const fn new() -> Answers<H> {
        Answers { shapes: Vec::new() } // alloc: none until a build of the checkout walks
    }

    fn at(&self, shape: &ContentId) -> Option<usize> {
        self.shapes.iter().position(|it| it.shape == *shape)
    }

    /// The answer kept of `shape`.
    #[cfg(test)]
    pub(super) fn answer(&self, shape: &ContentId) -> Option<&LastAnswer> {
        self.at(shape).and_then(|at| self.shapes[at].last.as_ref().ok())
    }

    /// The answer kept of `shape` with what the daemon holds beside it.
    pub(super) fn held_mut(&mut self, shape: &ContentId) -> Option<(&LastAnswer, &mut H)> {
        let at = self.at(shape)?;
        let slot = &mut self.shapes[at];
        match (&slot.last, &mut slot.held) {
            (Ok(answer), Some(held)) => Some((answer, held)),
            _ => None,
        }
    }

    /// Every answer kept, whatever its shape.
    pub(super) fn kept(&self) -> impl Iterator<Item = &LastAnswer> + '_ {
        self.shapes.iter().filter_map(|it| it.last.as_ref().ok())
    }

    /// The answer of the shape most recently used (the last a job of the checkout walked
    /// as, or was answered as), when one is kept of it: what the checkout's last build or
    /// check, sent again, is asked of. What is built ahead stops at (`serve::ahead`).
    pub(super) fn latest(&self) -> Option<&LastAnswer> {
        self.shapes.last().and_then(|it| it.last.as_ref().ok())
    }

    /// [`LastAnswer::held_bytes`] of every answer kept.
    pub(super) fn held_bytes(&self) -> usize {
        self.kept().map(LastAnswer::held_bytes).sum()
    }

    /// A job of `shape` walks: that shape's answer goes (the walk may leave the next), and
    /// no other shape's. Returns what was held beside each answer that went, for the daemon
    /// to let go.
    pub(super) fn begin(&mut self, shape: &ContentId) -> Vec<H> {
        let mut gone = Vec::new(); // alloc: none unless a view was held
        for slot in &mut self.shapes {
            if slot.shape == *shape || planted(Plant::DropsOthers) {
                slot.last = Err(Unkept::None);
                gone.extend(slot.held.take());
            }
        }
        gone
    }

    /// `last` is `shape`'s answer from now, or why it has none, with `held` beside it: the
    /// most recently used. Past [`MOST_SHAPES`] the least recently used shapes go. Returns
    /// what was held beside whatever went.
    pub(super) fn keep(&mut self, shape: ContentId, last: Last, held: Option<H>) -> Vec<H> {
        let mut gone = Vec::new(); // alloc: none unless a view was held
        if let Some(at) = self.at(&shape) {
            gone.extend(self.shapes.remove(at).held);
        }
        self.shapes.push(Shaped { shape, last, held });
        while self.shapes.len() > MOST_SHAPES {
            gone.extend(self.shapes.remove(0).held);
        }
        gone
    }

    /// What `asked`, a request of `shape`, is answered with: that shape's kept answer's
    /// responses and its `Work`, the shape made the most recently used and the view asked
    /// noted as one the answer was said again of ([`answered`]); or why the build walks.
    /// No other shape's answer is asked, and none is changed.
    pub(super) fn ask(
        &mut self,
        shape: &ContentId,
        asked: &Asked<'_>,
        disk: &impl Stamps,
    ) -> Result<(Arc<[Response]>, Option<Worked>), NotReplayed> {
        let Some(at) = self.at(shape) else {
            return Err(NotReplayed::NoAnswer(Unkept::None));
        };
        let found = ask(&self.shapes[at].last, asked, disk)?;
        let mut slot = self.shapes.remove(at);
        answered(&mut slot.last, asked.inputs.view);
        self.shapes.push(slot);
        Ok(found)
    }
}

#[cfg(test)]
mod tests;
