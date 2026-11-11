//! The no-op lane's core: when a build or a check of a checkout is answered as the checkout's
//! last one was, with no plan, no lowering, no task and no store ([`super::Jobs::begin`]
//! drives it; docs/cargo.md, "A build with nothing changed").
//!
//! An answer is kept ([`LastAnswer`]) only of a build that walked every unit and made
//! nothing ([`Recording::end`]), so what is said again is what a walk said of this very
//! state. It is said again only while every input of that walk is as it was
//! ([`LastAnswer::replayable`]), each input covered by one of:
//!
//! - **The checkout's files** (sources, manifests, the lock file, `.cargo/config.toml` in it,
//!   a build script's reads and listings in it, `include!`d files in it): the view's root
//!   ([`Inputs::view`]), which the checkout's watcher commits before the request is seen; or,
//!   where the root is another, each file that differs is a source the walk held by its code
//!   text alone, with the still text it had ([`LastAnswer::code_unmoved`], below).
//! - **The request**: its command, arguments, directory, whole environment and the config
//!   files outside the view by their bytes, the mounts (each source and path dependency
//!   outside the view by its root), the toolchain, where the daemon builds, and the blaze
//!   that serves it ([`Inputs::config`]); and what the request says past those (whether its
//!   diagnostics are colored, their width, how many refused functions it names, `-v`:
//!   [`Inputs::request`]).
//! - **Paths outside every view** (a build script's traced reads and the directories it
//!   listed, an `include!`d file, the toolchain's files, the C compiler's): the watches of
//!   [`blaze_vfs::outside`] heard nothing since the walk began ([`Vouches::quiet_since`]), and
//!   the walk looked at no such path on the disk ([`Walk::looks`]): each one it rested on was
//!   vouched for by a watch.
//! - **Paths outside the view that are mounted where they are** (a path dependency): each
//!   one's watch vouches for it since it was read ([`Asked::locals_vouched`]); else the build
//!   reads it again, and walks.
//! - **The outputs**: each directory they are in has been watched since before the walk and
//!   heard nothing since ([`Vouches::unchanged_since`]), so none was removed or replaced.
//! - **Paths the walk found absent that no watch can be confirmed of** (the directory of a
//!   build's layout profiles): each is looked at as the lane is asked, and is still absent
//!   ([`Vouches::absent`]).
//! - **Other jobs**: none is under way ([`Asked::alone`]).
//!
//! Events of the watches come after the changes that made them. The daemon holds every such
//! request until its checkouts' watchers' flushes and the round of the watches outside views
//! are in (`serve::heads`), and nothing vouches while a round is out, so a change made before
//! the request was sent is heard before this is asked.
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
//! A sans-IO core (rule 14), as `serve::why` is: it reads no file, no clock and no watch;
//! the daemon hands it the watches' word ([`Vouches`]), the files that differ ([`Edited`])
//! and the counts it read.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_api::{Response, Worked};
use blaze_ids::{ContentId, Hasher};
use blaze_units::query::BuildConfig;

/// The step a refusal of the lane is under in `blaze why` ([`blaze_kernel::Miss::step`]).
pub(super) const STEP: &str = "no-op answer";

/// What the watches of directories outside every view say ([`blaze_vfs::outside`]), as
/// [`LastAnswer::replayable`] asks it: the process's table in the daemon, a table of the
/// test's in a test.
pub(super) trait Vouches {
    /// [`blaze_vfs::outside::quiet_since`].
    fn quiet_since(&self, generation: u64) -> bool;
    /// [`blaze_vfs::outside::unchanged_since`].
    fn unchanged_since(&self, dir: &Path, generation: u64) -> bool;
    /// Whether nothing is at `path` now: a look at the disk, made as the lane is asked.
    fn absent(&self, path: &Path) -> bool;
}

/// What a build's answer is a function of that the daemon holds by value.
#[derive(Clone, Debug)]
pub(super) struct Inputs {
    /// The view's root: every file of the checkout.
    pub(super) view: ContentId,
    /// The view's whole build configuration.
    pub(super) config: Arc<BuildConfig>,
    /// The hash of `config`'s encoding.
    pub(super) config_id: ContentId,
    /// The hash of the request's work and its whole `Targets`: what `config` does not hold of
    /// it (colors, width, the refused functions named, verbosity).
    pub(super) request: ContentId,
}

impl Inputs {
    /// One id of all of them: what `blaze why` files a refusal under.
    pub(super) fn id(&self) -> ContentId {
        Hasher::new("blaze v2 serve no-op inputs")
            .id(&self.view)
            .id(&self.config_id)
            .id(&self.request)
            .finish()
    }

    /// The first input of `now` that is not as it is here, or none.
    pub(super) fn moved(&self, now: &Inputs) -> Option<Moved> {
        if self.files_moved(now) {
            return Some(Moved::Files);
        }
        self.moved_past_files(now)
    }

    /// Whether `now`'s view is another root than this one's.
    fn files_moved(&self, now: &Inputs) -> bool {
        self.view != now.view && !planted(Plant::IgnoresFiles)
    }

    /// [`Inputs::moved`] of everything but the view.
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

/// What of two configs that encode differently differs, the first found in this order.
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
    if let Some(name) = first_moved(&a.env, &b.env) {
        return Moved::Env(name);
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
    /// A variable of the request's environment, by name; never its values.
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
    /// What the request says past its config: colors, width, refused functions, verbosity.
    Rendering,
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
            Moved::Rendering => f.write_str(
                "how the request's diagnostics are said (colors, width, `--refused`, `-v`)",
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
    /// Its last build looked at a path outside every view on the disk, the last one this:
    /// no watch vouches for it, so nothing says it is as it was.
    Looked(Option<PathBuf>),
    /// An output of its last build is in no directory a watch can vouch for: right in the
    /// build directory, under neither build directory, or named through a symlink.
    Outputs,
    /// Its builds read this off the disk where the daemon counts no look and asks no watch.
    Uncounted(&'static str),
}

impl Unkept {
    /// The input no watch or key covers, as a noun: what `blaze why` names. None for a build
    /// whose answer is not kept for what it did, which the next build's is.
    fn input(&self) -> Option<String> {
        match self {
            Unkept::None | Unkept::Failed | Unkept::Made | Unkept::Planned => None,
            // alloc: a reason, once a refusal
            Unkept::Said(what) => Some(format!("what the last build said ({what})")),
            Unkept::Looked(Some(path)) => Some(format!(
                "{}, which no watch covers, so each build looks at it on the disk",
                path.display()
            )),
            Unkept::Looked(None) => Some(
                "a path outside the view which no watch covers, so each build looks at it on the disk"
                    .to_owned(),
            ),
            Unkept::Outputs => Some(
                "an output in no directory a watch can vouch for (right in the build directory, or named through a symlink)"
                    .to_owned(),
            ),
            Unkept::Uncounted(what) => {
                Some(format!("{what}, which each build reads off the disk and no watch covers"))
            }
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
            Unkept::Looked(Some(path)) => write!(
                f,
                "its last build looked at {} on the disk, which no watch covers",
                path.display()
            ),
            Unkept::Looked(None) => f.write_str(
                "its last build looked at a path outside every view on the disk, which no watch covers",
            ),
            Unkept::Outputs => {
                f.write_str("an output of its last build is in no directory a watch can vouch for")
            }
            Unkept::Uncounted(what) => {
                write!(f, "its builds read {what} off the disk, which no watch covers")
            }
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
    /// A path outside the view that is mounted where it is (a path dependency) has no watch
    /// vouching for it since it was read: the build reads it again.
    Locals,
    /// A change was heard under a watched directory outside every view since the answer's
    /// walk began, or a watch stopped vouching, or a round of flushes is out.
    Disturbed,
    /// Something under this directory of outputs changed or was removed since the answer's
    /// walk began, or it has not been watched since before then.
    OutputsMoved(PathBuf),
    /// This path, which the answer's walk found absent, is there now.
    Appeared(PathBuf),
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
                Some("a path dependency outside the checkout, which no watch covers".to_owned())
            }
            NotReplayed::Disturbed => {
                Some("a path outside the checkout that a build script or `include!` read, or its watch".to_owned())
            }
            NotReplayed::OutputsMoved(dir) => {
                Some(format!("the outputs under {}", dir.display()))
            }
            NotReplayed::Appeared(path) => Some(path.display().to_string()),
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
                "a path outside the view that is mounted where it is has no watch vouching for it",
            ),
            NotReplayed::Disturbed => f.write_str(
                "a change was heard outside the view since the kept build began, or a watch stopped vouching",
            ),
            NotReplayed::OutputsMoved(dir) => write!(
                f,
                "{} changed since the kept build began, or was not watched then",
                dir.display()
            ),
            NotReplayed::Appeared(path) => {
                write!(f, "{} is there now, and was not when the kept build ran", path.display())
            }
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
    /// Says an answer again whatever happened to the outputs (a deleted binary, `blaze clean`).
    IgnoresOutputs,
    /// Says an answer again whatever was heard outside the view (a file a build script
    /// traced, a file added to a directory it listed).
    IgnoresOutside,
    /// Says an answer of another view again (a source edit, the lock file).
    IgnoresFiles,
    /// Says an answer of another config again (a variable, a flag, a feature, the profile).
    IgnoresConfig,
    /// Says an answer of another rendering again (colors, width).
    IgnoresRequest,
    /// Says an answer again while another job may be writing the outputs.
    IgnoresOthers,
    /// Says an answer again though a path dependency outside the view is to be read again.
    IgnoresLocals,
    /// Says an answer again though a path its walk found absent is there now.
    IgnoresAppeared,
    /// Keeps the answer of a walk that looked at a path on the disk no watch covers.
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
}

#[cfg(test)]
thread_local! {
    /// The bug the test on this thread planted.
    pub(super) static PLANTED: std::cell::Cell<Option<Plant>> = const { std::cell::Cell::new(None) };
}

/// Whether the test on this thread planted `plant`; never outside a test.
fn planted(plant: Plant) -> bool {
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
    /// Every path outside the view that is mounted where it is on the disk is vouched for by
    /// its watch since it was read (`Mounts::stale_locals` names none).
    pub(super) locals_vouched: bool,
}

/// The answer of a checkout's last build that walked every unit and made nothing: what the
/// next build of the same inputs says again without walking, while nothing it rests on moved
/// ([`LastAnswer::replayable`]). Only such a build's answer is kept, never one that compiled,
/// placed or missed anything, so what is said again is what a walk said of this very state:
/// its warnings, its units and its artifacts as a build with nothing to do says them.
///
/// Kept in memory, per checkout, and dropped by the next job of the checkout that walks; a
/// restarted daemon keeps none, and walks.
pub(super) struct LastAnswer {
    pub(super) inputs: Inputs,
    /// [`blaze_vfs::outside::generation`] as read before the build looked at anything.
    pub(super) generation: u64,
    /// The directories it rests on the watches of ([`super::recorded_dirs`]): each one its
    /// outputs are in, canonical.
    pub(super) dirs: Arc<[PathBuf]>,
    /// The paths its walk found absent, which no watch can be confirmed of.
    pub(super) absent: Arc<[PathBuf]>,
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
}

/// Whether `diagnostic`, or one under it, names a file whose last name is `name`: in a span,
/// or in its rendered text (every snippet is under its file's path there).
fn names_file(diagnostic: &blaze_api::Diagnostic, name: &str) -> bool {
    diagnostic.rendered.contains(name)
        || diagnostic.spans.iter().any(|span| span.file.ends_with(name))
        || diagnostic.children.iter().any(|child| names_file(child, name))
}

impl LastAnswer {
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
        watches: &impl Vouches,
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
        if !asked.alone && !planted(Plant::IgnoresOthers) {
            return Err(NotReplayed::NotAlone);
        }
        if !asked.locals_vouched && !planted(Plant::IgnoresLocals) {
            return Err(NotReplayed::Locals);
        }
        if !watches.quiet_since(self.generation) && !planted(Plant::IgnoresOutside) {
            return Err(NotReplayed::Disturbed);
        }
        let moved = self.dirs.iter().find(|dir| !watches.unchanged_since(dir, self.generation));
        if let Some(dir) = moved
            && !planted(Plant::IgnoresOutputs)
        {
            return Err(NotReplayed::OutputsMoved(dir.clone())); // clone: a reason, once
        }
        if let Some(path) = self.absent.iter().find(|path| !watches.absent(path))
            && !planted(Plant::IgnoresAppeared)
        {
            return Err(NotReplayed::Appeared(path.clone())); // clone: a reason, once
        }
        Ok(())
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
    watches: &impl Vouches,
) -> Result<(Arc<[Response]>, Option<Worked>), NotReplayed> {
    match last {
        Err(unkept) => Err(NotReplayed::NoAnswer(unkept.clone())), // clone: a reason, once
        Ok(answer) => answer.replayable(asked, watches).map(|()| {
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
    /// [`blaze_vfs::outside::looks`] as it ended.
    pub(super) looks: u64,
    /// The last path looked at ([`blaze_vfs::outside::last_look`]), for the reason.
    pub(super) looked_at: Option<PathBuf>,
    /// Its `Work`.
    pub(super) worked: Option<Worked>,
}

/// A build that may become its checkout's [`LastAnswer`]: one that walks the checkout's last
/// plan. What it says is noted as the daemon tells it ([`Recording::hear`]), and it is kept
/// once it ends, if it made nothing ([`Recording::end`]).
pub(super) struct Recording {
    inputs: Inputs,
    /// [`blaze_vfs::outside::generation`] and [`blaze_vfs::outside::looks`] as read before
    /// it looked at anything.
    generation: u64,
    looks: u64,
    dirs: Vec<PathBuf>,
    absent: Vec<PathBuf>,
    responses: Vec<Response>,
    /// Why it is not kept, once something says so.
    refused: Option<Unkept>,
    /// [`LastAnswer::still`], as its front end vouched ([`Recording::vouch`]).
    still: Result<Arc<[StillFile]>, String>,
}

impl Recording {
    /// A recording of a build of `inputs` whose outputs are in `dirs` and which is to find
    /// nothing at each of `absent`, the watches' `generation` and the count of `looks` read
    /// before it looks at anything.
    pub(super) fn new(
        inputs: Inputs,
        generation: u64,
        looks: u64,
        dirs: Vec<PathBuf>,
        absent: Vec<PathBuf>,
    ) -> Recording {
        Recording {
            inputs,
            generation,
            looks,
            dirs,
            absent,
            responses: Vec::new(), // alloc: once a build that walks its checkout's last plan
            refused: None,
            // alloc: a reason, once a build that walks its checkout's last plan
            still: Err("its front end was not asked which sources it held by code text".to_owned()),
        }
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
    /// only what such a build says, and no path outside every view was looked at on the disk
    /// while it ran (the count of looks did not move: each such path it rested on was vouched
    /// for by a watch, which [`LastAnswer::replayable`] asks again). Else why it is not kept.
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
        if walk.looks != self.looks && !planted(Plant::KeepsALook) {
            return Err(Unkept::Looked(walk.looked_at));
        }
        Ok(LastAnswer {
            inputs: self.inputs,
            generation: self.generation,
            dirs: self.dirs.into(),
            absent: self.absent.into(),
            responses: self.responses.into(),
            worked: walk.worked,
            still: self.still,
            same: None,
        })
    }
}

#[cfg(test)]
mod tests;
