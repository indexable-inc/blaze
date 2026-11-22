//! sdk.rs's recorded xcrun answer under a deterministic simulation (rules 64, 78): a modeled
//! machine (the select link, three developer folders and their SDKs, two xcruns on `PATH`, a
//! wall clock) and the records' folder in a simulated bstd world (the records are written and
//! read through bstd as on the host) go through drawn histories of link switches, installs and
//! removals, SDKs retargeted and added, a shadowing xcrun, environment changes, a record torn,
//! and changes made WHILE xcrun runs. Before each command the oracle asks the model's xcrun
//! (the `Xcrun` way); the `Recorded` way must give the same `Result<Sdk, SdkRefused>`, whatever
//! it took from the record. The control plants a read list without the select link
//! ([`SdkPlant::SelectLinkUnread`]): the oracle must see a switched developer folder answered
//! from a stale record.
//!
//! Two environments that differ in one variable (`DEVELOPER_DIR`, or `TOOLCHAINS` alone) each
//! keep their own record: neither overwrites the other, so each starts xcrun once. Its control
//! plants a key without `TOOLCHAINS` ([`SdkPlant::ToolchainsUnkeyed`]): the two share one
//! record, and the oracle sees one answered with the other's folder.

use core::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use blaze_sim::{Broke, Config, Tape, check, control};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::{TASK_FRAME, Tag};
use bstd::test::BUDGET;

use super::*;

const BETA: &str = "/Applications/Xcode-beta.app/Contents/Developer";
const XCODE_SDKS: &str = "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs";
const XCODE_SDK: &str = "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk";
const BETA_SDKS: &str = "/Applications/Xcode-beta.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs";
const BETA_SDK: &str = "/Applications/Xcode-beta.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk";
const CLT_SDKS: &str = "/Library/Developer/CommandLineTools/SDKs";
const CLT_SDK: &str = "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk";
const USR_XCRUN: &str = "/usr/bin/xcrun";
const WRAP_XCRUN: &str = "/opt/wrap/xcrun";
const CUSTOM_SDK: &str = "/opt/custom/Custom.sdk";
const HOME: &str = "/Users/u";
/// A `TOOLCHAINS` the modeled xcrun answers with the custom SDK (a toolchain that brings its own).
const CUSTOM_TOOLCHAIN: &str = "org.custom.toolchain";
const DEVS: [&str; 3] = [XCODE_DEFAULT, BETA, COMMAND_LINE_TOOLS];
const PATHS: [&str; 5] = ["/usr/bin:/bin", "/opt/wrap:/usr/bin", "/usr/bin:/opt/wrap", "/bin", "bin:/usr/bin"];

/// One modeled file: its status, and what it names when it is a link.
#[derive(Clone, Copy, Debug)]
struct Node {
    stat: Stat,
    link: Option<&'static str>,
}

/// The modeled machine. Every change stamps the clock's now and a new inode where a file is
/// made again, as a file system does. The records are not its: they are the simulated world's
/// files, through [`Answers`].
struct Model {
    nodes: RefCell<BTreeMap<&'static str, Node>>,
    clock: Cell<u64>,
    inode: Cell<u64>,
    /// A change made while the next xcrun runs (after it read the state it answers from).
    during: Cell<Option<u64>>,
    /// How many times xcrun was started.
    started: Cell<u32>,
}

const DIR: u32 = 0o040_755;
const FILE_EXEC: u32 = 0o100_755;
const FILE_PLAIN: u32 = 0o100_644;
const LINK: u32 = 0o120_755;

impl Model {
    fn new() -> Model {
        let model = Model {
            nodes: RefCell::new(BTreeMap::new()),
            clock: Cell::new(1_000),
            inode: Cell::new(1),
            during: Cell::new(None),
            started: Cell::new(0),
        };
        model.make(SELECT_LINK, LINK, Some(XCODE_DEFAULT));
        for path in [XCODE_DEFAULT, XCODE_SDKS, XCODE_SDK, BETA, BETA_SDKS, BETA_SDK, COMMAND_LINE_TOOLS, CLT_SDKS, "/opt/custom", CUSTOM_SDK] {
            model.make(path, DIR, None);
        }
        model.make(USR_XCRUN, FILE_EXEC, None);
        model
    }

    fn tick(&self) -> u64 {
        let now = self.clock.get();
        self.clock.set(now + 1);
        now
    }

    /// `path` made (again): a new inode, its times now.
    fn make(&self, path: &'static str, mode: u32, link: Option<&'static str>) {
        let now = self.tick();
        let ino = self.inode.get();
        self.inode.set(ino + 1);
        self.nodes.borrow_mut().insert(path, Node { stat: Stat { dev: 1, ino, mode, mtime_ns: now, ctime_ns: now }, link });
    }

    fn remove(&self, path: &'static str) {
        self.nodes.borrow_mut().remove(path);
    }

    fn there(&self, path: &str) -> bool {
        self.nodes.borrow().contains_key(path)
    }

    /// `path` written in place: its times now.
    fn touch(&self, path: &str) {
        let now = self.tick();
        if let Some(node) = self.nodes.borrow_mut().get_mut(path) {
            node.stat.mtime_ns = now;
            node.stat.ctime_ns = now;
        }
    }

    /// A developer folder and its SDK folders installed, or removed when there.
    fn toggle(&self, dev: &'static str) {
        let all: [&'static str; 3] = match dev {
            XCODE_DEFAULT => [XCODE_DEFAULT, XCODE_SDKS, XCODE_SDK],
            BETA => [BETA, BETA_SDKS, BETA_SDK],
            _ => [COMMAND_LINE_TOOLS, CLT_SDKS, CLT_SDK],
        };
        if self.there(dev) {
            all.into_iter().for_each(|path| self.remove(path));
        } else {
            all.into_iter().for_each(|path| self.make(path, DIR, None));
        }
    }

    /// The change numbered `op` (below [`CHANGES`]).
    fn change(&self, op: u64, pick: u64) {
        let dev = DEVS[usize::try_from(pick % 3).unwrap_or(0)];
        match op {
            0 => self.make(SELECT_LINK, LINK, Some(dev)),
            1 => self.remove(SELECT_LINK),
            2 => self.toggle(dev),
            // The Command Line Tools' SDK link pointed at a newer SDK: made again.
            3 if self.there(CLT_SDK) => {
                self.make(CLT_SDK, LINK, Some(if pick % 2 == 0 { "MacOSX27.2.sdk" } else { "MacOSX26.5.sdk" }));
                self.touch(CLT_SDKS);
            }
            // An SDK added beside the chosen one: its folder written.
            4 => self.touch([XCODE_SDKS, BETA_SDKS, CLT_SDKS][usize::try_from(pick % 3).unwrap_or(0)]),
            5 if self.there(WRAP_XCRUN) => self.remove(WRAP_XCRUN),
            5 => self.make(WRAP_XCRUN, FILE_EXEC, None),
            // chmod: the shadowing xcrun made executable or not.
            6 => {
                let now = self.tick();
                if let Some(node) = self.nodes.borrow_mut().get_mut(WRAP_XCRUN) {
                    node.stat.mode = if node.stat.mode == FILE_EXEC { FILE_PLAIN } else { FILE_EXEC };
                    node.stat.ctime_ns = now;
                }
            }
            _ => {}
        }
    }

    /// What xcrun prints on this state: the shadowing one prints its own folder; the system's
    /// takes the custom SDK under [`CUSTOM_TOOLCHAIN`], else asks `DEVELOPER_DIR`, else the
    /// link, else Xcode, else the Command Line Tools.
    fn xcrun_now(&self, env: &XcrunEnv<'_>) -> Result<&'static str, &'static str> {
        let nodes = self.nodes.borrow();
        let executable = |path: &str| nodes.get(path).is_some_and(|node| node.stat.mode == FILE_EXEC);
        let mut which = None;
        for entry in env.path.unwrap_or("").split(':') {
            match entry {
                "/usr/bin" if executable(USR_XCRUN) => which = Some(USR_XCRUN),
                "/opt/wrap" if executable(WRAP_XCRUN) => which = Some(WRAP_XCRUN),
                _ => {}
            }
            if which.is_some() {
                break;
            }
        }
        match which {
            None => Err("xcrun: not found"),
            Some(WRAP_XCRUN) => Ok("/opt/custom/Custom.sdk\n"),
            Some(_) if env.toolchains == Some(CUSTOM_TOOLCHAIN) => Ok("/opt/custom/Custom.sdk\n"),
            Some(_) => {
                let link = nodes.get(SELECT_LINK).and_then(|node| node.link);
                let dev = match (env.developer_dir, link) {
                    (Some(dev), _) => DEVS.into_iter().find(|it| *it == dev).ok_or("an unknown DEVELOPER_DIR")?,
                    (None, Some(dev)) => dev,
                    (None, None) if nodes.contains_key(XCODE_DEFAULT) => XCODE_DEFAULT,
                    (None, None) => COMMAND_LINE_TOOLS,
                };
                if !nodes.contains_key(dev) {
                    return Err("xcrun: error: invalid active developer path");
                }
                let sdk = match dev {
                    XCODE_DEFAULT => XCODE_SDK,
                    BETA => BETA_SDK,
                    _ => CLT_SDK,
                };
                match nodes.contains_key(sdk) {
                    true => Ok(match sdk {
                        XCODE_SDK => "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk\n",
                        BETA_SDK => "/Applications/Xcode-beta.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk\n",
                        _ => "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk\n",
                    }),
                    false => Err("xcrun: error: SDK \"macosx\" cannot be located"),
                }
            }
        }
    }
}

/// The changes a step draws from.
const CHANGES: u64 = 7;

impl Machine for Model {
    fn stat(&self, path: &str, follow: bool, _frame: &Frame) -> Result<Option<Stat>, Unseen> {
        let nodes = self.nodes.borrow();
        let Some(node) = nodes.get(path) else { return Ok(None) };
        // Followed: the SDK link's target is a folder of the model's own, never one it names.
        Ok(Some(match (follow, node.link) {
            (true, Some(_)) => Stat { mode: DIR, ..node.stat },
            _ => node.stat,
        }))
    }

    fn readlink<'f>(&self, path: &str, _frame: &'f Frame) -> Result<Option<&'f str>, Unseen> {
        Ok(self.nodes.borrow().get(path).and_then(|node| node.link))
    }

    fn xcrun<'f>(&self, _cx: &Cx, env: &XcrunEnv<'_>, _frame: &'f Frame) -> Result<&'f str, XcrunFailed<'f>> {
        self.started.set(self.started.get() + 1);
        let answer = self.xcrun_now(env);
        // The change made while it ran, after it read its state: a record kept now would hold
        // reads that are not the state it answered from.
        if let Some(op) = self.during.take() {
            self.change(op % CHANGES, op / CHANGES);
        }
        answer.map_err(|why| XcrunFailed::Failed { why })
    }

    fn settings<'f>(&self, folder: &str, _frame: &'f Frame) -> Result<&'f str, Unread<'f>> {
        match self.there(folder) {
            true => Ok(r#"{"Version":"27.2"}"#),
            false => Err(Unread { why: "no such folder" }),
        }
    }

    fn system_version<'f>(&self, folder: &str, _frame: &'f Frame) -> Result<&'f str, Unread<'f>> {
        match self.there(folder) {
            true => Ok("<key>ProductBuildVersion</key><string>26B118</string>"),
            false => Err(Unread { why: "no such folder" }),
        }
    }

    fn wall_ns(&self) -> u64 {
        self.tick()
    }
}

/// The environment a step chose.
#[derive(Clone, Copy)]
struct Env {
    path: Option<&'static str>,
    developer_dir: Option<&'static str>,
    home: Option<&'static str>,
    toolchains: Option<&'static str>,
}

impl Env {
    fn xcrun(&self) -> XcrunEnv<'static> {
        XcrunEnv { path: self.path, developer_dir: self.developer_dir, home: self.home, toolchains: self.toolchains }
    }
}

/// The record of `env`'s key (with `plant`'s key) torn at a length the tape draws, when there
/// is one: a crash mid-write, or two writers. Written through bstd as a record is.
fn tear(tape: &Tape, cx: &Cx, fs: &Fs, env: &XcrunEnv<'_>, plant: SdkPlant, frame: &Frame) -> Result<(), Broke> {
    let Some(home) = env.home else { return Ok(()) };
    let answers = Answers::open(cx, fs, home).map_err(|error| Broke::Harness(format!("the records' folder: {error:?}")))?;
    let name = record_name(key_of(env, plant, frame)).map_err(|error| Broke::Harness(format!("a record's name: {error:?}")))?;
    let Some(bytes) = answers.read(cx, &name, frame).map_err(|error| Broke::Harness(format!("a record's read: {error:?}")))? else { return Ok(()) };
    let at = usize::try_from(tape.below(u64::try_from(bytes.len()).unwrap_or(0) + 1, "torn at")).unwrap_or(0);
    answers.write(cx, &name, &bytes[..at]).map_err(|error| Broke::Harness(format!("a record torn: {error:?}")))
}

/// One drawn history: up to forty steps of changes and commands, each command judged.
fn history(tape: &Tape, plant: SdkPlant) -> Result<(), Broke> {
    let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(|_| Broke::Harness("a root context".into()))?;
    let fs = io.need_fs().map_err(|_| Broke::Harness("the world's files".into()))?;
    let frame = Frame::with_reserve(1 << 20, Tag::Other).map_err(|_| Broke::Harness("the case's frame".into()))?;
    let model = Model::new();
    let mut env = Env { path: Some(PATHS[0]), developer_dir: None, home: Some(HOME), toolchains: None };
    let mut hits = 0u32;
    for step in 0..tape.below(40, "steps") {
        match tape.below(12, "step") {
            op @ 0..=6 => model.change(op, tape.below(6, "which")),
            7 => {
                env.path = tape.pick(&PATHS.map(Some), "PATH").copied().flatten();
                env.developer_dir = tape.pick(&[None, Some(XCODE_DEFAULT), Some(BETA), Some(COMMAND_LINE_TOOLS)], "DEVELOPER_DIR").copied().flatten();
                env.home = tape.pick(&[Some(HOME), None], "HOME").copied().flatten();
                env.toolchains = tape.pick(&[None, Some(CUSTOM_TOOLCHAIN)], "TOOLCHAINS").copied().flatten();
            }
            // The record torn at a drawn length (a crash mid-write, or two writers).
            8 => tear(tape, &cx, &fs, &env.xcrun(), plant, &frame)?,
            command => {
                if command == 11 {
                    model.during.set(Some(tape.below(CHANGES * 6, "changed while xcrun runs")));
                }
                let pending = model.during.take();
                let started = model.started.get();
                let (truth, _) = resolve_on(&cx, &fs, &model, None, &env.xcrun(), SdkFrom::Xcrun, NoPlant, &frame);
                model.started.set(started);
                model.during.set(pending);
                let (got, how) = resolve_on(&cx, &fs, &model, None, &env.xcrun(), SdkFrom::Recorded, plant, &frame);
                model.during.set(None);
                if how == SdkHow::Recorded {
                    hits += 1;
                }
                // The machine moved while this command's xcrun ran (after it read its state):
                // the command saw part of each state (xcrun's words from the one before, the
                // SDK folder's read from the one after: tape `2 0 1 11 9`, the beta folder
                // removed under it). Its answer is then xcrun's on either state, or a refusal
                // naming what it could not read; never a folder neither state gives. What the
                // move left in the record is judged by the next command, with nothing moving.
                let after = match pending {
                    Some(_) => {
                        let started = model.started.get();
                        let (after, _) = resolve_on(&cx, &fs, &model, None, &env.xcrun(), SdkFrom::Xcrun, NoPlant, &frame);
                        model.started.set(started);
                        Some(after)
                    }
                    None => None,
                };
                let moved_and_fits = after.as_ref().is_some_and(|after| got == *after || got.is_err());
                if got != truth && !moved_and_fits {
                    return Err(Broke::Oracle(format!(
                        "an answer other than xcrun's at step {step}: {got:?} by {how:?}, xcrun says {truth:?} (and on the state after the change made while it ran: {after:?})"
                    )));
                }
            }
        }
    }
    tape.note(format_args!("{hits} answered from the record"));
    Ok(())
}

/// Whatever the history, the recorded way answers as xcrun does, error or folder.
#[test]
fn the_recorded_answer_is_xcruns_answer() {
    check(Config::cases(400), |tape| history(tape, SdkPlant::None));
}

/// Control: a read list without the select link answers a switched developer folder from the
/// stale record, and the oracle says so.
#[test]
fn a_read_list_without_the_select_link_is_caught() {
    control(Config::cases(400), SdkPlant::None, SdkPlant::SelectLinkUnread, "an answer other than xcrun's", history);
}

/// Two environments that differ in one variable the tape picks (`DEVELOPER_DIR`, or `TOOLCHAINS`
/// alone), and two to nine commands among them on a machine where nothing moves: each command
/// answers as xcrun does, and each environment starts xcrun once, at its first command (a
/// second start is its record overwritten by the other's). Then both records are there, under
/// two names, each holding its own key.
fn two_environments(tape: &Tape, plant: SdkPlant) -> Result<(), Broke> {
    let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).map_err(|_| Broke::Harness("a root context".into()))?;
    let fs = io.need_fs().map_err(|_| Broke::Harness("the world's files".into()))?;
    let frame = Frame::with_reserve(1 << 20, Tag::Other).map_err(|_| Broke::Harness("the case's frame".into()))?;
    let model = Model::new();
    let base = XcrunEnv { path: Some(PATHS[0]), developer_dir: None, home: Some(HOME), toolchains: None };
    let other = match tape.below(2, "the variable that differs") {
        0 => XcrunEnv { developer_dir: Some(BETA), ..base },
        _ => XcrunEnv { toolchains: Some(CUSTOM_TOOLCHAIN), ..base },
    };
    let envs = [base, other];
    let mut asked = [false; 2];
    for command in 0..2 + tape.below(8, "more commands") {
        let which = usize::try_from(tape.below(2, "which environment")).unwrap_or(0);
        let env = &envs[which];
        let started = model.started.get();
        let (truth, _) = resolve_on(&cx, &fs, &model, None, env, SdkFrom::Xcrun, NoPlant, &frame);
        model.started.set(started);
        let (got, how) = resolve_on(&cx, &fs, &model, None, env, SdkFrom::Recorded, plant, &frame);
        if got != truth {
            return Err(Broke::Oracle(format!("an answer other than xcrun's at command {command}, environment {which}: {got:?} by {how:?}, xcrun says {truth:?}")));
        }
        let first = !asked[which];
        asked[which] = true;
        if (how == SdkHow::Recorded) == first {
            return Err(Broke::Oracle(format!(
                "a record overwritten: command {command}, environment {which} ({}) was {how:?}",
                if first { "its first" } else { "not its first" }
            )));
        }
    }
    let names = [record_name(key_of(&envs[0], plant, &frame)), record_name(key_of(&envs[1], plant, &frame))];
    let answers = Answers::open(&cx, &fs, HOME).map_err(|error| Broke::Harness(format!("the records' folder: {error:?}")))?;
    for (which, env) in envs.iter().enumerate() {
        if !asked[which] {
            continue;
        }
        let name = names[which].as_ref().map_err(|error| Broke::Harness(format!("a record's name: {error:?}")))?;
        let bytes = answers.read(&cx, name, &frame).map_err(|error| Broke::Harness(format!("a record's read: {error:?}")))?;
        let holds = bytes.and_then(|bytes| bytes.get(4..36)).is_some_and(|key| key == key_of(env, plant, &frame).0);
        if !holds {
            return Err(Broke::Oracle(format!("a record overwritten: environment {which}'s file does not hold its key")));
        }
    }
    if asked == [true, true] && names[0] == names[1] {
        return Err(Broke::Oracle("a record overwritten: two environments, one file".into()));
    }
    Ok(())
}

/// Two environments, two records: neither overwrites the other.
#[test]
fn two_environments_keep_two_records() {
    check(Config::cases(200), |tape| two_environments(tape, SdkPlant::None));
}

/// Control: a key without `TOOLCHAINS` gives two environments that differ only there one
/// record, and one is answered with the other's folder.
#[test]
fn a_key_without_toolchains_is_caught() {
    control(Config::cases(200), SdkPlant::None, SdkPlant::ToolchainsUnkeyed, "an answer other than xcrun's", two_environments);
}

/// The record is taken with no process when nothing moved, and a switch of the select link is
/// a typed stale read that asks xcrun once and records again. Prints the three phases.
#[test]
fn a_second_command_starts_no_xcrun_and_a_switch_starts_one() {
    let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("the world's files");
    let frame = Frame::with_reserve(1 << 20, Tag::Other).expect("the test's frame");
    let model = Model::new();
    let env = XcrunEnv { path: Some(PATHS[0]), developer_dir: None, home: Some(HOME), toolchains: None };
    let run = || resolve_on(&cx, &fs, &model, None, &env, SdkFrom::Recorded, NoPlant, &frame).1;

    assert_eq!(run(), SdkHow::Asked { why: Ask::NoRecord, kept: Kept::Kept });
    // The record is a file of the home's records folder, named by its key, of this form.
    let answers = Answers::open(&cx, &fs, HOME).expect("the records' folder");
    let name = record_name(key_of(&env, NoPlant, &frame)).expect("a record's name");
    let bytes = answers.read(&cx, &name, &frame).expect("the record's read").expect("the record of this environment's key");
    assert!(bytes.starts_with(MAGIC), "a record of this form");
    assert_eq!(run(), SdkHow::Recorded, "nothing moved, and xcrun was started again");
    assert_eq!(model.started.get(), 1);
    model.change(0, 1);
    assert_eq!(run(), SdkHow::Asked { why: Ask::Stale(Read::SelectLink), kept: Kept::Kept });
    assert_eq!(run(), SdkHow::Recorded);
    assert_eq!(model.started.get(), 2);
    // A change while xcrun runs: answered, not recorded (the racy rule).
    model.change(0, 0);
    model.during.set(Some(4));
    let how = run();
    assert!(matches!(how, SdkHow::Asked { why: Ask::Stale(Read::SelectLink), kept: Kept::Racy(_) | Kept::Moved(_) }), "{how:?}");
    println!("{}\n{}\n{}", SdkHow::Recorded.phase(), SdkHow::Asked { why: Ask::Stale(Read::SelectLink), kept: Kept::Kept }.phase(), how.phase());
}
