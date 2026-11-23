//! The macOS SDK, resolved once at the command's start (rule 75): the edge half of
//! blaze-apple-sdk's [`blaze_apple_sdk::resolve`], which decides. Here `SDKROOT` is the value
//! `main` read from the command's environment; only when it is unset is xcrun asked, with the
//! command's `PATH`, `DEVELOPER_DIR`, `HOME` and `TOOLCHAINS` ([`XcrunEnv::vars`]: its whole
//! environment), by [`XCRUN_DEADLINE`] (rule 18), killed past
//! it; and the one folder chosen has its `SDKSettings.json` read. What comes out is a value
//! the request carries ([`crate::fetch::FetchEnv::sdk`]): the native build-script runner takes
//! its folder and the scripts' keys its identity (scripts.rs); nothing deeper reads the machine
//! for it. A refusal fails only what needs the SDK (a script placed native), by name.
//!
//! **xcrun's answer as a record with a read list** (SPEC E, 2026-11-21). Starting xcrun is a
//! process on every command (4.9 ms on tempo, the phase "open checkout: the macOS SDK (xcrun
//! started: SDKROOT is unset)"); its answer depends on what this module can stat instead:
//!
//! - its inputs: the program and its arguments, and every variable it is started with
//!   ([`XcrunEnv::vars`], the one list the spawn and the key both read): the record's key
//!   ([`key_of`]), computed before xcrun runs, and the record's file name, so another
//!   environment is another record and never overwrites this one (rule 21);
//! - the developer folder `xcode-select` chose: the link [`SELECT_LINK`] (its target and its
//!   own status), and, read when it is absent, whether [`XCODE_DEFAULT`] and
//!   [`COMMAND_LINE_TOOLS`] are there;
//! - which `xcrun` runs: the first `<PATH entry>/xcrun` that is an executable file, and its
//!   status (a `PATH` with a relative entry is not recorded: it names the working folder);
//! - the folder it printed: its own status and link target, and its parent's status, the SDKs
//!   folder it was chosen from (an SDK added, removed or renamed there moves it).
//!
//! Each command reads its record (bstd: the records' folder opened beneath the home, a stat, one
//! sized read) and the list again (a readlink and a few stats, about 0.1 ms; among them one
//! `lstat` of the SDK folder the record names, so a folder an Xcode upgrade removed reads
//! unequal and is refused by name, [`Read::Answer`], then asked again) and takes the
//! recorded answer when every read is equal ([`Read`] names the first that is not); else it
//! asks xcrun and records the new answer. `SDKSettings.json` and `SystemVersion.plist` are read
//! every command either way, so the identity is always today's.
//!
//! **Why a recorded answer is xcrun's.** A record is written only when (1) the reads that do
//! not depend on the answer were equal before xcrun started and after it ended, (2) every
//! status read has its times before the wall clock at xcrun's start (the racy rule: a change
//! during or after xcrun's run shows as a time at or after it), and (3) the answer folder is
//! there. So the read list holds the state xcrun answered from, and a later change to any of
//! it differs on the next command's reads. What xcrun reads beyond these (its own cache,
//! `xcrun_db`) answers as it would; an SDK changed in place moves the identity, which is read
//! every command. [`Kept`] says why an answer was not recorded.
//!
//! Two ways to one answer behind one seam (rule 78), [`SdkFrom`]: `Recorded` (shipped) and
//! `Xcrun` (ask every command), held equal by tests/sdk_tests.rs over drawn histories of link
//! switches, installs, SDKs added and removed, a `PATH` that shadows xcrun, environments
//! switched, and changes made while xcrun runs; its plants drop the link from the list
//! ([`SdkPlant::SelectLinkUnread`]) and `TOOLCHAINS` from the key ([`SdkPlant::ToolchainsUnkeyed`]).
//!
//! The records are files of the home's folder `<HOME>/.blaze/v2/sdk-answers` ([`ANSWERS`]), one
//! per key, named by its first 16 bytes in hex ([`record_name`]), form [`MAGIC`], holding the
//! whole key (a file of this name holding another key is refused as [`Ask::OtherInputs`]).
//! They are read and published through bstd (a stat, one sized read; a publish, unsynced, by
//! rename: a reader sees the old record or the new whole). The form before 2026-11-21 was one
//! file `<HOME>/.blaze/v2/sdk` for every environment; it is never read.

use core::fmt;

use blaze_apple_sdk::{SETTINGS, SYSTEM_VERSION, Sdk, SdkRefused, Unread, XcrunFailed};
use blaze_ids::ContentId;
use blaze_own::Vec;
use blaze_own::planted::{NoPlant, Planted, Plants};
use bstd::Cx;
use bstd::clock::Clock;
use bstd::fs::{Content, Dir, Durable, Fs, FsError, HomeError, ReadWrite, SizedError};
use bstd::mem::{Bytes, Frame};
use bstd::path::{Path, PathError};

/// What the edge read of the command's environment for xcrun: `PATH` (where `xcrun` is
/// found), `DEVELOPER_DIR` (which developer folder xcrun asks), `HOME`, and `TOOLCHAINS` (the
/// toolchain xcrun selects). xcrun is started with these and nothing else of the command's
/// environment, so they are all of its environment, and the record's key covers them all.
pub(crate) struct XcrunEnv<'e> {
    pub(crate) path: Option<&'e str>,
    pub(crate) developer_dir: Option<&'e str>,
    pub(crate) home: Option<&'e str>,
    pub(crate) toolchains: Option<&'e str>,
}

impl<'e> XcrunEnv<'e> {
    /// The variables xcrun is started with, by name: the one list the spawn ([`xcrun`]) passes
    /// and the key ([`key_of`]) digests, so neither can leave out what the other has.
    pub(crate) fn vars(&self) -> [(&'static str, Option<&'e str>); 4] {
        [("PATH", self.path), ("DEVELOPER_DIR", self.developer_dir), ("HOME", self.home), ("TOOLCHAINS", self.toolchains)]
    }
}

/// The link `xcode-select --switch` writes: the developer folder chosen.
pub(crate) const SELECT_LINK: &str = "/var/db/xcode_select_link";
/// The developer folder taken when no link is there and Xcode is installed.
pub(crate) const XCODE_DEFAULT: &str = "/Applications/Xcode.app/Contents/Developer";
/// The developer folder taken when no link is there and Xcode is not.
pub(crate) const COMMAND_LINE_TOOLS: &str = "/Library/Developer/CommandLineTools";
/// The records' folder, beneath the home: one file per key ([`record_name`]).
pub(crate) const ANSWERS: &str = ".blaze/v2/sdk-answers";
/// The record's form.
pub(crate) const MAGIC: &[u8; 4] = b"BSK2";
const KEY_DOMAIN: &str = "blaze-cli sdk: xcrun, its arguments and its environment 2";
/// The most bytes of a record read: a few paths and a few statuses.
const RECORD_MOST: u64 = 64 * 1024;

/// Which way the folder is found with `SDKROOT` unset (rule 78): two ways to one answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SdkFrom {
    /// xcrun started every command.
    Xcrun,
    /// xcrun's answer recorded with its read list, taken while the list reads equal.
    Recorded,
}

/// The bug a control plants in the read list (blaze_own::planted, D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SdkPlant {
    None,
    /// The read list leaves out [`SELECT_LINK`]: a developer folder switched is not seen.
    SelectLinkUnread,
    /// The key leaves out `TOOLCHAINS`: two environments that differ only there share one
    /// record, and the second overwrites the first.
    ToolchainsUnkeyed,
}

impl Plants for SdkPlant {}

/// One read of the list, in its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Read {
    SelectLink = 1,
    XcodeDefault = 2,
    CommandLineTools = 3,
    Xcrun = 4,
    Answer = 5,
    AnswerParent = 6,
}

impl Read {
    fn of(byte: u8) -> Option<Read> {
        [Read::SelectLink, Read::XcodeDefault, Read::CommandLineTools, Read::Xcrun, Read::Answer, Read::AnswerParent].into_iter().find(|read| *read as u8 == byte)
    }
}

impl fmt::Display for Read {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Read::SelectLink => SELECT_LINK,
            Read::XcodeDefault => XCODE_DEFAULT,
            Read::CommandLineTools => COMMAND_LINE_TOOLS,
            Read::Xcrun => "the xcrun on PATH",
            Read::Answer => "the SDK folder xcrun printed",
            Read::AnswerParent => "the folder of SDKs it was chosen from",
        })
    }
}

/// A record's folder, read or publish that bstd refused: the folder, the file or the call, and
/// why (each names its path).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub(crate) enum RecordError {
    /// The records' folder beneath the home could not be opened.
    Home(HomeError),
    /// A record's name is no path (a bug: [`ANSWERS`] and hex names are paths).
    Path(PathError),
    /// A stat or the publish failed.
    Fs(FsError),
    /// The sized read failed, or the file moved under it.
    Read(SizedError),
    /// The file at the record's name is a folder, or longer than any record ([`RECORD_MOST`]).
    NotARecord { len: u64 },
}

/// Why xcrun was asked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Ask {
    /// [`SdkFrom::Xcrun`].
    Always,
    /// `HOME` is unset or not absolute: no record has a place.
    NoHome,
    /// `PATH` is unset or has a relative entry: which xcrun runs depends on the working folder.
    PathNotAbsolute,
    /// No record of this key: the first command of this environment.
    NoRecord,
    /// The record could not be read (or its folder opened): why, naming it. Answered by xcrun,
    /// and recorded again when the folder can be written.
    RecordUnread(RecordError),
    /// A record of another form.
    OtherForm,
    /// The file of this key's name holds another key: a 128-bit prefix shared, or a file not
    /// written by this module.
    OtherInputs,
    /// A read of its list differs today.
    Stale(Read),
    /// A read of its list could not be made today: the system's error number.
    Unseen { read: Read, errno: u16 },
}

/// Whether xcrun's answer was recorded, and why not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Kept {
    Kept,
    /// Recorded never ([`SdkFrom::Xcrun`]).
    NotAsked,
    /// xcrun failed: nothing to record.
    XcrunFailed,
    /// No place for a record (no home), or a `PATH` with a relative entry.
    NoPlace,
    /// It printed what is not an absolute path (the resolve refuses it).
    NotAbsolute,
    /// A read could not be made: the system's error number.
    Unseen { read: Read, errno: u16 },
    /// A read that does not depend on the answer moved while xcrun ran.
    Moved(Read),
    /// A status read has a time at or after xcrun's start (the racy rule).
    Racy(Read),
    /// The folder it printed is not there.
    AnswerAbsent,
    /// Writing the record failed: why, naming the file.
    Unwritten(RecordError),
}

/// How the command's SDK folder was found: the profile's phase name ([`SdkHow::phase`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum SdkHow {
    /// `SDKROOT` named it (or nothing was asked: the resolve failed before).
    SdkRoot,
    /// The recorded answer, its read list equal: no process.
    Recorded,
    /// xcrun started.
    Asked { why: Ask, kept: Kept },
}

impl SdkHow {
    /// The profile's phase for this way (rule 17): which way, and why xcrun was asked.
    pub(crate) fn phase(&self) -> &'static str {
        match self {
            SdkHow::SdkRoot => "open checkout: the macOS SDK (SDKROOT's, its settings read)",
            SdkHow::Recorded => "open checkout: the macOS SDK (xcrun's recorded answer, its read list equal: no process)",
            SdkHow::Asked { kept: Kept::Kept, why } => match why {
                Ask::NoRecord => "open checkout: the macOS SDK (xcrun started: no record; recorded)",
                Ask::Stale(Read::SelectLink) => "open checkout: the macOS SDK (xcrun started: xcode_select_link moved; recorded)",
                Ask::Stale(Read::Xcrun) => "open checkout: the macOS SDK (xcrun started: another xcrun on PATH; recorded)",
                Ask::Stale(Read::Answer | Read::AnswerParent) => "open checkout: the macOS SDK (xcrun started: the SDK folders moved; recorded)",
                Ask::Stale(_) => "open checkout: the macOS SDK (xcrun started: a developer folder came or went; recorded)",
                Ask::OtherInputs => "open checkout: the macOS SDK (xcrun started: the record of this key's name held another key; recorded)",
                Ask::RecordUnread(_) => "open checkout: the macOS SDK (xcrun started: the record could not be read; recorded)",
                Ask::OtherForm => "open checkout: the macOS SDK (xcrun started: a record of another form; recorded)",
                _ => "open checkout: the macOS SDK (xcrun started; recorded)",
            },
            SdkHow::Asked { why: Ask::Always, .. } => "open checkout: the macOS SDK (xcrun started: asked every command)",
            SdkHow::Asked { kept: Kept::XcrunFailed, .. } => "open checkout: the macOS SDK (xcrun started, and failed)",
            SdkHow::Asked { kept: Kept::NoPlace, .. } => "open checkout: the macOS SDK (xcrun started: no HOME, or a PATH with a relative entry: not recorded)",
            SdkHow::Asked { kept: Kept::Racy(_) | Kept::Moved(_), .. } => "open checkout: the macOS SDK (xcrun started; not recorded: a read moved while it ran)",
            SdkHow::Asked { kept: Kept::Unwritten(_), .. } => "open checkout: the macOS SDK (xcrun started; not recorded: the write failed)",
            SdkHow::Asked { .. } => "open checkout: the macOS SDK (xcrun started; not recorded: a read could not be made, or the answer is no folder)",
        }
    }
}

/// A file's status as the read list compares it: equal statuses of one path at two reads say
/// it was not replaced, renamed, written or changed between them (every change moves `ctime`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Stat {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) mode: u32,
    /// Ns since the Unix epoch, on the file system's clock (the wall clock's).
    pub(crate) mtime_ns: u64,
    pub(crate) ctime_ns: u64,
}

impl Stat {
    fn executable_file(&self) -> bool {
        self.mode & 0o170_000 == 0o100_000 && self.mode & 0o111 != 0
    }
}

/// A read the system refused: its error number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Unseen {
    pub(crate) errno: u16,
}

/// The machine the SDK is found on: the host's ([`Host`]), or a test's model. Every read of
/// the system is handed in here (rule 75); [`recorded`] decides from what they return. The
/// records are not the machine's: they are files of the command's [`Fs`] ([`Answers`]), one
/// way on the host and in the simulation.
pub(crate) trait Machine {
    /// `path`'s status (the link itself unless `follow`); `None` when it is not there.
    fn stat(&self, path: &str, follow: bool, frame: &Frame) -> Result<Option<Stat>, Unseen>;
    /// What the link `path` names; `None` when it is no link or not there.
    fn readlink<'f>(&self, path: &str, frame: &'f Frame) -> Result<Option<&'f str>, Unseen>;
    /// What `xcrun --sdk macosx --show-sdk-path` printed, run by its deadline.
    fn xcrun<'f>(&self, cx: &Cx, env: &XcrunEnv<'_>, frame: &'f Frame) -> Result<&'f str, XcrunFailed<'f>>;
    /// The text of `<folder>/SDKSettings.json`.
    fn settings<'f>(&self, folder: &str, frame: &'f Frame) -> Result<&'f str, Unread<'f>>;
    /// The text of `<folder>/System/Library/CoreServices/SystemVersion.plist`.
    fn system_version<'f>(&self, folder: &str, frame: &'f Frame) -> Result<&'f str, Unread<'f>>;
    /// The wall clock now, ns since the Unix epoch.
    fn wall_ns(&self) -> u64;
}

/// The command's macOS SDK: `sdkroot`, else xcrun's answer (recorded under the home in `fs`, or
/// asked, by `from`), its settings read from the folder chosen; the words of a failure held in
/// `frame`; and how.
pub(crate) fn resolve_how<'f>(cx: &Cx, fs: &Fs, sdkroot: Option<&'f str>, env: &XcrunEnv<'_>, from: SdkFrom, clock: &Clock, frame: &'f Frame) -> (Result<Sdk<'f>, SdkRefused<'f>>, SdkHow) {
    let cx = cx.span("sdk: the macOS SDK folder");
    resolve_on(&cx, fs, &Host { clock }, sdkroot, env, from, NoPlant, frame)
}

/// [`resolve_how`] on `machine`, its records in `fs`, with `plant`.
#[expect(clippy::too_many_arguments, reason = "the resolve's inputs, each one handle; the test's seam adds the machine and the plant")]
pub(crate) fn resolve_on<'f, M: Machine, P: Planted<SdkPlant>>(
    cx: &Cx,
    fs: &Fs,
    machine: &M,
    sdkroot: Option<&'f str>,
    env: &XcrunEnv<'_>,
    from: SdkFrom,
    plant: P,
    frame: &'f Frame,
) -> (Result<Sdk<'f>, SdkRefused<'f>>, SdkHow) {
    let mut how = SdkHow::SdkRoot;
    let sdk = blaze_apple_sdk::resolve(
        sdkroot,
        || {
            let (folder, asked) = match from {
                SdkFrom::Xcrun => (machine.xcrun(cx, env, frame), SdkHow::Asked { why: Ask::Always, kept: Kept::NotAsked }),
                SdkFrom::Recorded => recorded(cx, fs, machine, env, plant, frame),
            };
            how = asked;
            folder
        },
        |folder| machine.settings(folder, frame),
        |folder| machine.system_version(folder, frame),
    );
    (sdk, how)
}

/// The key of everything xcrun's answer is a function of besides the read list: the program,
/// its arguments, and each variable it is started with ([`XcrunEnv::vars`]), by name, presence
/// and bytes. Computed from text before xcrun runs (rule 65). `plant`
/// [`SdkPlant::ToolchainsUnkeyed`] leaves `TOOLCHAINS` out.
fn key_of<P: Planted<SdkPlant>>(env: &XcrunEnv<'_>, plant: P, frame: &Frame) -> ContentId {
    let mut bytes = Vec::new_in(frame);
    let mut put = |text: &str| {
        bytes.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
    };
    put(blaze_apple_sdk::XCRUN);
    for arg in blaze_apple_sdk::XCRUN_ARGS {
        put(arg);
    }
    for (name, value) in env.vars() {
        if name == "TOOLCHAINS" && plant.has(SdkPlant::ToolchainsUnkeyed) {
            continue;
        }
        put(name);
        match value {
            None => put("\u{0}unset"),
            Some(value) => {
                put("\u{0}set");
                put(value);
            }
        }
    }
    ContentId::of(KEY_DOMAIN, &bytes)
}

/// The record's file name of `key` in [`ANSWERS`]: its first 16 bytes in hex (the record holds
/// the whole key, checked on read).
pub(crate) fn record_name(key: ContentId) -> Result<Path, PathError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = [0u8; 32];
    for (at, byte) in key.0[..16].iter().enumerate() {
        name[2 * at] = HEX[usize::from(byte >> 4)];
        name[2 * at + 1] = HEX[usize::from(byte & 15)];
    }
    // Hex digits are UTF-8.
    Path::new(core::str::from_utf8(&name).unwrap_or_default())
}

/// The folder of xcrun's recorded answers, `<HOME>/.blaze/v2/sdk-answers`, through bstd: a
/// record read by a stat and one sized read, written by an unsynced publish (a temporary name
/// renamed over: a reader sees the old record or the new, whole). Two commands writing one key
/// at once leave either's record; one torn by a crash is not a whole record, refused on read:
/// a reader takes a record only when its key and whole list equal today's, so neither is ever
/// a wrong answer, only a miss.
pub(crate) struct Answers {
    dir: Dir<ReadWrite>,
}

impl Answers {
    /// The folder beneath the absolute `home`, made by its first record.
    pub(crate) fn open(cx: &Cx, fs: &Fs, home: &str) -> Result<Answers, RecordError> {
        let folder = Path::new(ANSWERS).map_err(RecordError::Path)?;
        fs.home(cx, home, &folder).map(|dir| Answers { dir }).map_err(RecordError::Home)
    }

    /// The bytes of the record `name`, held in `frame`; `None` when there is none.
    pub(crate) fn read<'f>(&self, cx: &Cx, name: &Path, frame: &'f Frame) -> Result<Option<&'f [u8]>, RecordError> {
        let Some(meta) = self.dir.stat(cx, name).map_err(RecordError::Fs)? else { return Ok(None) };
        if meta.dir || meta.len > RECORD_MOST {
            return Err(RecordError::NotARecord { len: meta.len });
        }
        let mut held = Bytes::new_in(frame, "sdk: xcrun's recorded answer");
        self.dir.read_sized(cx, name, meta.len, &mut held).map_err(RecordError::Read)?;
        Ok(Some(frame.alloc_slice_copy(held.as_slice())))
    }

    /// `bytes` made the record `name`, published whole.
    pub(crate) fn write(&self, cx: &Cx, name: &Path, bytes: &[u8]) -> Result<(), RecordError> {
        self.dir.publish(cx, name, Content::bytes(bytes), Durable::No).map(|_| ()).map_err(RecordError::Fs)
    }
}

/// Whether every entry of `path` is absolute (which xcrun runs does not depend on the
/// working folder).
fn path_absolute(path: Option<&str>) -> bool {
    path.is_some_and(|path| path.split(':').all(|entry| entry.starts_with('/')))
}

/// One read's bytes appended to `out`: its tag, its length, then `payload`'s.
fn segment(out: &mut Vec<u8, &Frame>, read: Read, payload: impl FnOnce(&mut Vec<u8, &Frame>)) {
    out.push(read as u8);
    let at = out.len();
    out.extend_from_slice(&[0, 0]);
    payload(out);
    // A status and a path: under 64 KiB.
    let len = u16::try_from(out.len() - at - 2).unwrap_or(u16::MAX);
    out[at..at + 2].copy_from_slice(&len.to_le_bytes());
}

fn put_stat(out: &mut Vec<u8, &Frame>, stat: Option<Stat>) {
    match stat {
        None => out.push(0),
        Some(stat) => {
            out.push(1);
            out.extend_from_slice(&stat.dev.to_le_bytes());
            out.extend_from_slice(&stat.ino.to_le_bytes());
            out.extend_from_slice(&stat.mode.to_le_bytes());
            out.extend_from_slice(&stat.mtime_ns.to_le_bytes());
            out.extend_from_slice(&stat.ctime_ns.to_le_bytes());
        }
    }
}

fn put_text(out: &mut Vec<u8, &Frame>, text: Option<&str>) {
    match text {
        None => out.push(0),
        Some(text) => {
            out.push(1);
            out.extend_from_slice(&u16::try_from(text.len()).unwrap_or(u16::MAX).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
    }
}

/// What reading the list found beside its bytes: the newest time of any status read, and
/// whether the answer folder is there.
struct Listed {
    newest: Option<(u64, Read)>,
    answer_there: bool,
}

impl Listed {
    fn saw(&mut self, read: Read, stat: Option<Stat>) {
        if let Some(stat) = stat {
            let ns = stat.mtime_ns.max(stat.ctime_ns);
            if self.newest.is_none_or(|(newest, _)| ns > newest) {
                self.newest = Some((ns, read));
            }
        }
    }
}

/// The read list appended to `out`, in [`Read`]'s order: the reads of the developer folder and
/// of xcrun, then, given `answer`, those of the folder it printed. `Err` names the read the
/// system refused.
fn reads<M: Machine, P: Planted<SdkPlant>>(machine: &M, env: &XcrunEnv<'_>, answer: Option<&str>, plant: P, out: &mut Vec<u8, &Frame>, frame: &Frame) -> Result<Listed, (Read, Unseen)> {
    let mut listed = Listed { newest: None, answer_there: false };
    let stat = |read: Read, path: &str, follow: bool| machine.stat(path, follow, frame).map_err(|unseen| (read, unseen));
    let link = |read: Read, path: &str| machine.readlink(path, frame).map_err(|unseen| (read, unseen));
    if !plant.has(SdkPlant::SelectLinkUnread) {
        let (status, target) = (stat(Read::SelectLink, SELECT_LINK, false)?, link(Read::SelectLink, SELECT_LINK)?);
        listed.saw(Read::SelectLink, status);
        segment(out, Read::SelectLink, |out| {
            put_stat(out, status);
            put_text(out, target);
        });
    }
    for (read, path) in [(Read::XcodeDefault, XCODE_DEFAULT), (Read::CommandLineTools, COMMAND_LINE_TOOLS)] {
        let status = stat(read, path, false)?;
        listed.saw(read, status);
        segment(out, read, |out| put_stat(out, status));
    }
    // The xcrun a spawn by name runs: the first executable file of that name on PATH.
    let mut found = None;
    for (index, entry) in env.path.unwrap_or("").split(':').enumerate() {
        let candidate = bstd::mem::format_in!(frame, "{}/xcrun", entry.trim_end_matches('/'));
        if let Some(status) = stat(Read::Xcrun, candidate.as_str(), true)?.filter(Stat::executable_file) {
            found = Some((index, status));
            break;
        }
    }
    listed.saw(Read::Xcrun, found.map(|(_, status)| status));
    segment(out, Read::Xcrun, |out| {
        out.extend_from_slice(&u16::try_from(found.map_or(u16::MAX.into(), |(index, _)| index)).unwrap_or(u16::MAX).to_le_bytes());
        put_stat(out, found.map(|(_, status)| status));
    });
    if let Some(answer) = answer {
        let answer = answer.trim_end_matches('/');
        let (status, target) = (stat(Read::Answer, answer, false)?, link(Read::Answer, answer)?);
        listed.saw(Read::Answer, status);
        listed.answer_there = status.is_some();
        segment(out, Read::Answer, |out| {
            put_stat(out, status);
            put_text(out, target);
        });
        let parent = answer.rsplit_once('/').map_or("/", |(parent, _)| if parent.is_empty() { "/" } else { parent });
        let status = stat(Read::AnswerParent, parent, false)?;
        listed.saw(Read::AnswerParent, status);
        segment(out, Read::AnswerParent, |out| put_stat(out, status));
    }
    Ok(listed)
}

/// The first read of a list: its tag, its bytes, and the list after it; `None` when the
/// bytes do not hold a whole read.
fn next_segment(bytes: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = bytes.split_first()?;
    let len = usize::from(u16::from_le_bytes([*rest.first()?, *rest.get(1)?]));
    let payload = rest.get(2..2 + len)?;
    Some((tag, payload, &rest[2 + len..]))
}

/// Whether `list` is whole reads to its end.
fn whole_list(mut list: &[u8]) -> bool {
    while !list.is_empty() {
        match next_segment(list) {
            Some((_, _, rest)) => list = rest,
            None => return false,
        }
    }
    true
}

/// The first read whose bytes differ between the recorded list and today's; `None` when the
/// two are equal to their ends. A read on one side only, or bytes that are not whole, differ.
fn first_difference(mut recorded: &[u8], mut today: &[u8]) -> Option<Read> {
    loop {
        if recorded.is_empty() && today.is_empty() {
            return None;
        }
        match (next_segment(recorded), next_segment(today)) {
            (Some((a, x, after_a)), Some((b, y, after_b))) if a == b && x == y => {
                recorded = after_a;
                today = after_b;
            }
            (Some((tag, _, _)), _) | (None, Some((tag, _, _))) => return Some(Read::of(tag).unwrap_or(Read::SelectLink)),
            // Bytes that are not whole on both sides: the first read stands for them.
            (None, None) => return Some(Read::SelectLink),
        }
    }
}

/// A record's answer and read list, when it is this form and of `key`.
fn decode(bytes: &[u8], key: ContentId) -> Result<(&str, &[u8]), Ask> {
    let header = 4 + 32 + 2;
    if bytes.len() < header || !bytes.starts_with(MAGIC) {
        return Err(Ask::OtherForm);
    }
    if bytes[4..36] != key.0 {
        return Err(Ask::OtherInputs);
    }
    let len = usize::from(u16::from_le_bytes([bytes[36], bytes[37]]));
    let answer = bytes.get(header..header + len).and_then(|it| core::str::from_utf8(it).ok()).ok_or(Ask::OtherForm)?;
    let list = &bytes[header + len..];
    if !whole_list(list) {
        return Err(Ask::OtherForm);
    }
    Ok((answer, list))
}

/// xcrun's answer from the record whose read list reads equal today, else asked and recorded.
fn recorded<'f, M: Machine, P: Planted<SdkPlant>>(cx: &Cx, fs: &Fs, machine: &M, env: &XcrunEnv<'_>, plant: P, frame: &'f Frame) -> (Result<&'f str, XcrunFailed<'f>>, SdkHow) {
    let Some(home) = env.home.filter(|home| home.starts_with('/')) else { return ask(cx, machine, env, plant, None, Ask::NoHome, frame) };
    if !path_absolute(env.path) {
        return ask(cx, machine, env, plant, None, Ask::PathNotAbsolute, frame);
    }
    // The key, from the environment's text before anything is read (rule 65): it names the
    // record's file, so each environment reads and writes its own.
    let key = key_of(env, plant, frame);
    let (place, why) = {
        let read_cx = cx.span("sdk: xcrun's recorded answer, its read list read");
        let opened = match Answers::open(&read_cx, fs, home) {
            Ok(answers) => record_name(key).map(|name| (answers, name)).map_err(RecordError::Path),
            Err(error) => Err(error),
        };
        match opened {
            Err(error) => (None, Ask::RecordUnread(error)),
            Ok((answers, name)) => {
                let why = match answers.read(&read_cx, &name, frame) {
                    Err(error) => Ask::RecordUnread(error),
                    Ok(None) => Ask::NoRecord,
                    // The record is validated before its answer is used: its form, its whole
                    // key, its whole list; then every read of the list today, the SDK folder's
                    // own status among them (an Xcode upgrade that removed it reads unequal).
                    Ok(Some(bytes)) => match decode(bytes, key) {
                        Err(why) => why,
                        Ok((answer, list)) => {
                            let mut today = Vec::new_in(frame);
                            match reads(machine, env, Some(answer), plant, &mut today, frame) {
                                Err((read, Unseen { errno })) => Ask::Unseen { read, errno },
                                Ok(_) => match first_difference(list, &today) {
                                    None => return (Ok(answer), SdkHow::Recorded),
                                    Some(read) => Ask::Stale(read),
                                },
                            }
                        }
                    },
                };
                (Some((answers, name)), why)
            }
        }
    };
    ask(cx, machine, env, plant, place.as_ref().map(|(answers, name)| (answers, name, key)), why, frame)
}

/// xcrun asked, and its answer recorded at `place` when the module's three conditions hold.
fn ask<'f, M: Machine, P: Planted<SdkPlant>>(
    cx: &Cx,
    machine: &M,
    env: &XcrunEnv<'_>,
    plant: P,
    place: Option<(&Answers, &Path, ContentId)>,
    why: Ask,
    frame: &'f Frame,
) -> (Result<&'f str, XcrunFailed<'f>>, SdkHow) {
    let wall_before = machine.wall_ns();
    let mut before = Vec::new_in(frame);
    let pre = reads(machine, env, None, plant, &mut before, frame);
    let printed = match machine.xcrun(cx, env, frame) {
        Ok(printed) => printed,
        Err(failed) => return (Err(failed), SdkHow::Asked { why, kept: Kept::XcrunFailed }),
    };
    let kept = match place {
        None => Kept::NoPlace,
        Some(place) => {
            let keep_cx = cx.span("sdk: xcrun's answer recorded with its read list");
            keep(&keep_cx, machine, env, plant, place, (wall_before, pre.map(|_| ()), &before[..]), printed.trim(), frame)
        }
    };
    (Ok(printed), SdkHow::Asked { why, kept })
}

/// The record of `answer` written, when the reads before xcrun (`before`, from `wall_before`)
/// and after it allow (the module's three conditions); why not, else.
#[expect(clippy::too_many_arguments, reason = "the keep's inputs, each one handle or one read of xcrun's run")]
fn keep<M: Machine, P: Planted<SdkPlant>>(
    cx: &Cx,
    machine: &M,
    env: &XcrunEnv<'_>,
    plant: P,
    (answers, name, key): (&Answers, &Path, ContentId),
    (wall_before, pre, before): (u64, Result<(), (Read, Unseen)>, &[u8]),
    answer: &str,
    frame: &Frame,
) -> Kept {
    if let Err((read, Unseen { errno })) = pre {
        return Kept::Unseen { read, errno };
    }
    if !answer.starts_with('/') {
        return Kept::NotAbsolute;
    }
    let mut after = Vec::new_in(frame);
    let listed = match reads(machine, env, Some(answer), plant, &mut after, frame) {
        Ok(listed) => listed,
        Err((read, Unseen { errno })) => return Kept::Unseen { read, errno },
    };
    // (1) the reads that do not depend on the answer did not move while xcrun ran.
    if let Some(read) = first_difference(before, after.get(..before.len()).unwrap_or(&after[..])) {
        return Kept::Moved(read);
    }
    // (2) the racy rule.
    if let Some((ns, read)) = listed.newest
        && ns >= wall_before
    {
        return Kept::Racy(read);
    }
    // (3) the folder is there.
    if !listed.answer_there {
        return Kept::AnswerAbsent;
    }
    let mut record = Vec::with_capacity_in(4 + 32 + 2 + answer.len() + after.len(), frame);
    record.extend_from_slice(MAGIC);
    record.extend_from_slice(&key.0);
    record.extend_from_slice(&u16::try_from(answer.len()).unwrap_or(u16::MAX).to_le_bytes());
    record.extend_from_slice(answer.as_bytes());
    record.extend_from_slice(&after);
    match answers.write(cx, name, &record) {
        Ok(()) => Kept::Kept,
        Err(error) => Kept::Unwritten(error),
    }
}

/// The host: its file system, its xcrun, its wall clock (handed in as `clock`).
pub(crate) struct Host<'c> {
    clock: &'c Clock,
}

/// Ns since the epoch of a status's seconds and nanoseconds; 0 before it.
fn ns_of(secs: i64, nanos: i64) -> u64 {
    u64::try_from(secs).map_or(0, |secs| secs.saturating_mul(1_000_000_000).saturating_add(u64::try_from(nanos).unwrap_or(0)))
}

/// A read the system refused, by its error number (0 when it gave none).
fn unseen(error: &std::io::Error) -> Unseen {
    Unseen { errno: error.raw_os_error().and_then(|it| u16::try_from(it).ok()).unwrap_or(0) }
}

/// Whether `error` says the path is not there (or a folder on it is not a folder).
fn not_there(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

// The read list's reads go through std's `metadata`, `symlink_metadata` and `read_link`, as
// `settings` does: one `stat`/`lstat`/`readlink` each. bstd's `Meta` has no mode (xcrun's
// executable bit) and it reads no link's target, and its `Fs` reaches an absolute path only through a second driver
// (`Fs::home`); these are fixed absolute paths of the system, read at the command's edge.
// The records are bstd's ([`Answers`]).
impl Machine for Host<'_> {
    fn stat(&self, path: &str, follow: bool, _frame: &Frame) -> Result<Option<Stat>, Unseen> {
        use std::os::unix::fs::MetadataExt as _;
        let read = if follow { std::fs::metadata(path) } else { std::fs::symlink_metadata(path) };
        match read {
            Ok(meta) => Ok(Some(Stat {
                dev: meta.dev(),
                ino: meta.ino(),
                mode: meta.mode(),
                mtime_ns: ns_of(meta.mtime(), meta.mtime_nsec()),
                ctime_ns: ns_of(meta.ctime(), meta.ctime_nsec()),
            })),
            Err(error) if not_there(&error) => Ok(None),
            Err(error) => Err(unseen(&error)),
        }
    }

    fn readlink<'f>(&self, path: &str, frame: &'f Frame) -> Result<Option<&'f str>, Unseen> {
        match std::fs::read_link(path) {
            // A target that is not UTF-8 is held by its lossy form: equal targets give equal
            // text, and the link's own status is read beside it.
            Ok(target) => Ok(Some(held(frame, format_args!("{}", target.display())))),
            // Not there, or there and no link (EINVAL).
            Err(error) if not_there(&error) || error.kind() == std::io::ErrorKind::InvalidInput => Ok(None),
            Err(error) => Err(unseen(&error)),
        }
    }

    fn xcrun<'f>(&self, cx: &Cx, env: &XcrunEnv<'_>, frame: &'f Frame) -> Result<&'f str, XcrunFailed<'f>> {
        xcrun(cx, env, frame)
    }

    fn settings<'f>(&self, folder: &str, frame: &'f Frame) -> Result<&'f str, Unread<'f>> {
        stamp(folder, SETTINGS, frame)
    }

    fn system_version<'f>(&self, folder: &str, frame: &'f Frame) -> Result<&'f str, Unread<'f>> {
        stamp(folder, SYSTEM_VERSION, frame)
    }

    fn wall_ns(&self) -> u64 {
        self.clock.wall().as_ns()
    }
}

/// `words` held in `frame`.
fn held<'f>(frame: &'f Frame, words: core::fmt::Arguments<'_>) -> &'f str {
    let text = bstd::mem::format_in!(frame, "{words}");
    crate::work::held_str(text.as_str(), frame)
}

/// The text of `<folder>/<file>` (an SDK's settings or build file), held in `frame`.
fn stamp<'f>(folder: &str, file: &str, frame: &'f Frame) -> Result<&'f str, Unread<'f>> {
    let path = held(frame, format_args!("{folder}/{file}"));
    match std::fs::read(path) {
        Ok(bytes) => match core::str::from_utf8(&bytes) {
            Ok(text) => Ok(crate::work::held_str(text, frame)),
            Err(_) => Err(Unread { why: "it is not UTF-8" }),
        },
        Err(error) => Err(Unread { why: held(frame, format_args!("{error}")) }),
    }
}

/// What `xcrun --sdk macosx --show-sdk-path` printed, run by its deadline: started linked in a
/// group of its own, so a kill at the deadline ends all it started.
#[cfg(target_os = "macos")]
fn xcrun<'f>(cx: &Cx, env: &XcrunEnv<'_>, frame: &'f Frame) -> Result<&'f str, XcrunFailed<'f>> {
    use std::io::Read as _;

    use blaze_apple_sdk::{XCRUN, XCRUN_ARGS, XCRUN_DEADLINE};
    use blaze_core::linked::{Exit, Link, Piped, Waited, start_linked};
    use blaze_core::wait::Deadline;

    let _cx = cx.span("sdk: xcrun --sdk macosx --show-sdk-path");
    let failed = |words: core::fmt::Arguments<'_>| XcrunFailed::Failed { why: held(frame, words) };
    // Its whole environment, nothing else of this process's: the variables xcrun reads, the
    // list the record's key digests ([`XcrunEnv::vars`]).
    let mut vars = std::vec::Vec::new_in(std::alloc::Global);
    for (name, value) in env.vars() {
        if let Some(value) = value {
            vars.push((std::ffi::OsString::from(name), std::ffi::OsString::from(value)));
        }
    }
    let command = blaze_core::Command {
        program: std::path::PathBuf::from(XCRUN),
        args: XCRUN_ARGS.iter().map(std::ffi::OsString::from).collect(),
        env: vars,
        dir: std::path::PathBuf::from("/"),
        stdin: std::vec::Vec::new_in(std::alloc::Global),
        sandbox: blaze_core::Sandbox::Off,
        memory: None,
    };
    let Piped { child, stdout: out, stderr: err } =
        start_linked(&command, &Link { inherit: &[], own_group: true }).map_err(|error| failed(format_args!("{error}")))?;
    let ended = child.wait_until(Deadline::at(XCRUN_DEADLINE)).map_err(|error| failed(format_args!("{error}")))?;
    let read = |fd: std::os::fd::OwnedFd| {
        let mut bytes = std::vec::Vec::new_in(std::alloc::Global);
        std::fs::File::from(fd).read_to_end(&mut bytes).map(|_| bytes)
    };
    let exit = match ended {
        Waited::Running(child) => {
            child.kill_group().map_err(|error| failed(format_args!("{error}")))?;
            return Err(XcrunFailed::Expired { after: XCRUN_DEADLINE });
        }
        Waited::Ended(exit) => exit,
    };
    match exit {
        Exit::Signaled(signal) => Err(XcrunFailed::Signaled { signal }),
        Exit::Exited(0) => {
            let bytes = read(out).map_err(|error| failed(format_args!("reading its output: {error}")))?;
            core::str::from_utf8(&bytes).map(|text| crate::work::held_str(text, frame)).map_err(|_| XcrunFailed::NotText)
        }
        Exit::Exited(status) => {
            let bytes = read(err).map_err(|error| failed(format_args!("reading its error stream: {error}")))?;
            Err(XcrunFailed::Exited { status, said: held(frame, format_args!("{}", std::string::String::from_utf8_lossy(&bytes).trim())) })
        }
    }
}

/// No xcrun off macOS: `SDKROOT` alone names an SDK there.
#[cfg(not(target_os = "macos"))]
fn xcrun<'f>(_cx: &Cx, _env: &XcrunEnv<'_>, _frame: &'f Frame) -> Result<&'f str, XcrunFailed<'f>> {
    Err(XcrunFailed::NotMacos)
}

/// The way the host finds the SDK with `SDKROOT` unset: recorded on macOS; elsewhere xcrun's
/// refusal at once (there is none to record).
pub(crate) const SHIPPED: SdkFrom = if cfg!(target_os = "macos") { SdkFrom::Recorded } else { SdkFrom::Xcrun };

/// A command's SDK in a test that runs no native script: none, said so.
#[cfg(test)]
pub(crate) const NONE_IN_TESTS: Result<Sdk<'static>, SdkRefused<'static>> =
    Err(SdkRefused::Neither { xcrun: XcrunFailed::Failed { why: "a test gives no macOS SDK" } });

#[cfg(test)]
#[path = "tests/sdk_tests.rs"]
mod tests;
