//! The no-op lane against a walk: a model checkout, its watches and its builds, driven by any
//! run of edits and requests. The lane's own code decides ([`ask`], [`Recording`]); the model
//! is what a daemon gives it (the view's root, the config, the watches' word) and what a
//! walk of every unit would say.

use std::collections::BTreeMap;

use blaze_api::{Diagnostic, Level};
use blaze_units::query::{Command, Invocation, Mount, Origin};
use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use super::*;

/// The directories of the checkout's outputs, the `dev` profile's and `release`'s.
const OUT: [&str; 2] = ["/b/debug", "/b/release"];
/// A directory outside the view a watch can be of, where the build script reads a header.
const INCLUDE: &str = "/usr/include";
/// The path dependency outside the view.
const DEP: &str = "/dep";
/// The directory of layout profiles, which is there or not.
const LAYOUT: &str = "/b/layout";
/// A directory's `since` while its watch is not confirmed.
const UNCONFIRMED: u64 = u64::MAX;
/// The checkout's source, a file of its one crate, by its path from the view's root.
const SOURCE: &str = "src/lib.rs";
/// A Rust file of the checkout the build script reads by its bytes.
const HELD: &str = "gen/input.rs";
/// The lock file.
const LOCK: &str = "Cargo.lock";

/// Everything a build of the model's checkout reads, and its output.
#[derive(Clone, Debug)]
struct World {
    /// Two files of the checkout, by content: a source (its tokens) and the lock file.
    source: u8,
    lock: u8,
    /// The text of a `//` comment at the end of a line of the source: in no code text, so in
    /// no key of a unit keyed by code text, and in the line a diagnostic quotes.
    comment: u8,
    /// The text of such a comment in [`HELD`], which the build script reads by its bytes.
    held_comment: u8,
    /// Whether the crate's unit is keyed by its code text (not when a lint of it reads
    /// comments: clippy's, then keyed by its files' bytes).
    narrow: bool,
    /// Whether the build has a warning that quotes the source's commented line.
    warns: bool,
    /// A header outside the view the build script read, under [`INCLUDE`].
    header: u8,
    /// How many files a directory the build script listed holds, when it lists one: no watch
    /// vouches for a listing, so each walk looks at it on the disk.
    lists: bool,
    listed: u8,
    /// A variable the build reads, and one it does not.
    cc: u8,
    shlvl: u8,
    features: u8,
    release: bool,
    /// Whether the request's diagnostics are colored.
    color: bool,
    /// The path dependency's content, and whether its directory is one a watch vouches for
    /// (not when it is named through a link).
    dep: u8,
    dep_watched: bool,
    /// Whether the layout profiles' directory is there.
    layout: bool,
    /// What each profile's output file holds, when it is in place: the answer it was made
    /// from.
    outputs: [Option<String>; 2],
    /// What each profile's build script's `OUT_DIR` holds, under the profile's directory of
    /// outputs: what the script made of the header, or what a foreign process wrote there.
    out_dirs: [Option<String>; 2],
    /// Another job is under way, which may write the outputs at any time.
    other_job: bool,
    /// A watch wanted in one request is in place only at the end of the next, after that
    /// one's walk (the daemon's hold waits for no watch being made); else as the next begins.
    lag: bool,
}

impl World {
    fn new(lists: bool, dep_watched: bool, lag: bool) -> World {
        World {
            source: 0,
            lock: 0,
            comment: 0,
            held_comment: 0,
            narrow: true,
            warns: false,
            header: 0,
            lists,
            listed: 0,
            cc: 0,
            shlvl: 0,
            features: 0,
            release: false,
            color: false,
            dep: 0,
            dep_watched,
            layout: false,
            outputs: [None, None],
            out_dirs: [None, None],
            other_job: false,
            lag,
        }
    }

    /// Where the profile asked for places its output.
    fn out(&self) -> &'static str {
        OUT[usize::from(self.release)]
    }

    /// The output of the profile asked for.
    fn output(&mut self) -> &mut Option<String> {
        &mut self.outputs[usize::from(self.release)]
    }

    /// What the build script makes in its `OUT_DIR`: a function of the header it reads and
    /// of the bytes of [`HELD`].
    fn generated(&self) -> String {
        format!("generated from header {} and input {}", self.header, self.held_comment)
    }

    /// What the build's outputs are made from: every input a build reads, and none it does
    /// not (`shlvl`; the source's comment, for a unit keyed by code text).
    fn made_from(&self) -> String {
        let listed = if self.lists { self.listed } else { 0 };
        // A lint that reads comments reads this one.
        let read = if self.narrow { 0 } else { self.comment };
        format!(
            "source {} read {read} input {} lock {} header {} listed {listed} cc {} features {} release {} color {} dep {} layout {}",
            self.source,
            self.held_comment,
            self.lock,
            self.header,
            self.cc,
            self.features,
            self.release,
            self.color,
            self.dep,
            self.layout,
        )
    }

    /// What a walk of every unit says of this state, with nothing to make: what its outputs
    /// are made from and, when it warns, the source's line as it is now, comment and all,
    /// under the file's name.
    fn answer(&self) -> String {
        match self.warns {
            true => format!("{}\n --> {SOURCE} | code // {}", self.made_from(), self.comment),
            false => self.made_from(),
        }
    }

    /// The checkout's files as a view holds them: what its root is of.
    fn files(&self) -> [u8; 4] {
        [self.source, self.comment, self.held_comment, self.lock]
    }
}

/// The still text of a Rust file whose tokens are `tokens`: nothing of its comments.
fn still_of(tokens: u8) -> ContentId {
    id(&format!("still {tokens}"))
}

/// The files that differ between two views of the checkout, as the daemon reads them: each
/// Rust one with its still texts, the lock file with none.
fn edits(was: [u8; 4], now: [u8; 4]) -> Vec<FileEdit> {
    let mut edits = Vec::new();
    if was[3] != now[3] {
        edits.push(FileEdit { path: LOCK.to_owned(), still: None });
    }
    if was[2] != now[2] {
        // Its tokens never change in the model: only its comment does.
        edits.push(FileEdit { path: HELD.to_owned(), still: Some((still_of(0), still_of(0))) });
    }
    if was[0] != now[0] || was[1] != now[1] {
        let still = Some((still_of(was[0]), still_of(now[0])));
        edits.push(FileEdit { path: SOURCE.to_owned(), still });
    }
    edits
}

/// The watches outside views, as [`blaze_vfs::outside`] keeps them: a generation, the last
/// disturbance, each directory's `since` and `changed`, and the changes made that no round
/// has delivered yet.
#[derive(Default)]
struct Watches {
    generation: u64,
    disturbed: u64,
    dirs: BTreeMap<&'static str, (u64, u64)>,
    pending: Vec<&'static str>,
    layout: bool,
}

impl Watches {
    fn want(&mut self, dir: &'static str) {
        self.dirs.entry(dir).or_insert((UNCONFIRMED, 0));
    }

    fn confirmed(&self, dir: &str) -> bool {
        self.dirs.get(dir).is_some_and(|it| it.0 != UNCONFIRMED)
    }

    /// Something under `dir` changed: heard at the next round when its watch is in place,
    /// never when it is not.
    fn change(&mut self, dir: &'static str) {
        if self.confirmed(dir) {
            self.pending.push(dir);
        }
    }

    /// A round of flushes: every change made before it is heard.
    fn round(&mut self) {
        for dir in std::mem::take(&mut self.pending) {
            self.generation += 1;
            self.disturbed = self.generation;
            if let Some(it) = self.dirs.get_mut(dir) {
                it.1 = self.generation;
            }
        }
    }

    /// The directories wanted whose watch is not in place.
    fn unconfirmed(&self) -> Vec<&'static str> {
        self.dirs.iter().filter(|(_, it)| it.0 == UNCONFIRMED).map(|(dir, _)| *dir).collect()
    }

    /// The watch of each of `dirs` is in place from now.
    fn confirm(&mut self, dirs: &[&'static str]) {
        for dir in dirs {
            if let Some(it) = self.dirs.get_mut(dir) {
                self.generation += 1;
                it.0 = self.generation;
            }
        }
    }
}

impl Vouches for Watches {
    fn quiet_since(&self, generation: u64) -> bool {
        self.disturbed <= generation
    }

    fn unchanged_since(&self, dir: &Path, generation: u64) -> bool {
        let dir = dir.to_str().expect("the model's directories are UTF-8");
        self.dirs.get(dir).is_some_and(|it| it.0 <= generation && it.1 <= generation)
    }

    fn absent(&self, _: &Path) -> bool {
        !self.layout
    }
}

/// An input a run changes.
#[derive(Clone, Copy, Debug)]
enum Input {
    Source,
    Lock,
    Header,
    Listed,
    Cc,
    Shlvl,
    Features,
    Profile,
    Color,
    Dependency,
    Layout,
    /// The `//` comment at the end of a line of the source says something else.
    Comment,
    /// The `//` comment in the file the build script reads says something else.
    HeldComment,
}

/// A step of a run.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// The input gets another value.
    Edit(Input),
    /// The header is written with the bytes it has: the watch hears a change of nothing.
    Touch,
    DeleteOutput,
    /// A foreign process writes into the build script's `OUT_DIR`.
    EditOutDir,
    /// The store lets go of what the outputs were made from: the next walk makes them
    /// again, and finds the files in place as it would place them.
    Evict,
    /// Another job starts, or the one under way ends.
    OtherJob,
    /// A request. With `tamper`, the output is deleted while its walk runs, after the walk
    /// looked at it.
    Build { tamper: bool },
}

/// A bug planted: one of the lane's ([`Plant`]), or the daemon's hold skipped, so a request
/// is asked of the lane before the watches' round is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bug {
    Lane(Plant),
    NoRound,
    /// A build script reused writes its `OUT_DIR` again on every walk, in place or not.
    ReplayWrites,
    /// Every change heard under a directory of outputs after a walk that wrote there is
    /// taken for that walk's own echo, a foreign write made in the same window too.
    EchoHidesForeign,
}

fn id(text: &str) -> ContentId {
    Hasher::new("the no-op lane's test").part(text.as_bytes()).finish()
}

fn said(text: &str) -> Response {
    Response::Diagnostic(Diagnostic {
        level: Level::Error,
        code: None,
        unit: None,
        facts: Vec::new(),
        spans: Vec::new(),
        message: text.to_owned(),
        children: Vec::new(),
        rendered: text.to_owned(),
        explanation: None,
    })
}

fn text_of(responses: &[Response]) -> String {
    let texts = responses.iter().map(|it| match it {
        Response::Diagnostic(diagnostic) => diagnostic.rendered.as_str(),
        _ => "a response the model never says",
    });
    texts.collect::<Vec<_>>().join("\n")
}

/// The model's daemon: the lane's state for the checkout, and what a daemon holds beside it.
struct Daemon {
    last: Last,
    /// The key of the checkout's last plan: its view and config.
    plan: Option<ContentId>,
    /// What the store holds each profile's output made from.
    known: [Option<String>; 2],
    watches: Watches,
    looks: u64,
    /// The watches' generation read before the header was last looked at on the disk.
    header_read: Option<u64>,
    /// The path dependency as it was last read, and the watches' generation read before.
    dep_read: u8,
    dep_generation: u64,
    /// The header and the input each profile's kept script run is of, and what this daemon left in each
    /// `OUT_DIR` (the files' identity and their stamps as one: a foreign write moves them).
    script_of: [Option<(u8, u8)>; 2],
    left: [Option<String>; 2],
    /// The last walk wrote under a directory of outputs: its echo is yet to be heard.
    echo: bool,
    /// How many requests the lane answered.
    answered: u32,
    /// The files of the view the last walk that may be kept read: what the daemon holds a
    /// view of, to read what differs from it.
    walked: Option<[u8; 4]>,
}

impl Daemon {
    fn new() -> Daemon {
        Daemon {
            last: Err(Unkept::None),
            plan: None,
            known: [None, None],
            watches: Watches::default(),
            looks: 0,
            header_read: None,
            dep_read: 0,
            dep_generation: 0,
            script_of: [None, None],
            left: [None, None],
            echo: false,
            answered: 0,
            walked: None,
        }
    }

    /// The request's inputs as the daemon holds them: the view's root, the config with the
    /// path dependency as it was last read, and how the request is rendered.
    fn inputs(&self, world: &World) -> Inputs {
        let mut args = vec![format!("--features=f{}", world.features)];
        if world.release {
            args.push("--release".to_owned());
        }
        let config = BuildConfig {
            invocation: Invocation {
                command: Command::Build,
                args,
                root: "/w".to_owned(),
                cwd: "/w".to_owned(),
                env: vec![
                    ("CC".to_owned(), world.cc.to_string()),
                    ("SHLVL".to_owned(), world.shlvl.to_string()),
                ],
                files: Vec::new(),
            },
            mounts: vec![
                Mount {
                    at: DEP.to_owned(),
                    origin: Origin::Root(id(&format!("dep {}", self.dep_read))),
                    under: String::new(),
                },
                Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
            ],
            host: "h".to_owned(),
            library: "/rustc/x/library".to_owned(),
            std: "/s/std".to_owned(),
            builds: "/s/build".to_owned(),
            sources: "/s/sources".to_owned(),
            blaze: "0.0.0-dev".to_owned(),
        };
        let config_id = Hasher::new("the no-op lane's test config").part(&config.encode()).finish();
        Inputs {
            view: id(&format!("view {:?}", world.files())),
            config: Arc::new(config),
            config_id,
            request: id(&format!("request {}", world.color)),
        }
    }

    /// Whether the path dependency's watch vouches for what was read of it.
    fn dep_vouched(&self, world: &World) -> bool {
        world.dep_watched && self.watches.unchanged_since(Path::new(DEP), self.dep_generation)
    }

    /// A request: held for the watches' round, asked of the lane, and walked when the lane
    /// refuses. The oracle is asked of an answer said again.
    fn request(&mut self, world: &mut World, tamper: bool, bug: Option<Bug>) -> Result<(), String> {
        if bug == Some(Bug::EchoHidesForeign) && self.echo {
            self.watches.pending.retain(|dir| !OUT.contains(dir));
        }
        self.echo = false;
        if bug != Some(Bug::NoRound) {
            self.watches.round();
        }
        let wanted = self.watches.unconfirmed();
        if !world.lag {
            self.watches.confirm(&wanted);
        }
        self.watches.layout = world.layout;
        let inputs = self.inputs(world);
        // What the daemon reads of the files, only where the answer's view is another root.
        let read = match (&self.last, self.walked) {
            (Ok(answer), Some(was)) if !answer.files_known(&inputs.view) => {
                Some(edits(was, world.files()))
            }
            _ => None,
        };
        let asked = Asked {
            inputs: &inputs,
            edited: match &read {
                Some(edits) => Edited::Files(edits),
                None => Edited::Unread("the roots are one, or no view is held"),
            },
            traced: false,
            alone: !world.other_job,
            locals_vouched: self.dep_vouched(world),
        };
        let found = ask(&self.last, &asked, &self.watches);
        if found.is_ok() {
            answered(&mut self.last, inputs.view);
        }
        match found {
            Ok((responses, _)) => {
                self.answered += 1;
                let (again, walk) = (text_of(&responses), world.answer());
                if again != walk {
                    return Err(format!("said again: `{again}`, where a walk says `{walk}`"));
                }
                if world.output().is_none() {
                    return Err("said again with the output gone".to_owned());
                }
                if world.other_job {
                    return Err("said again while another job may write the outputs".to_owned());
                }
                if world.out_dirs[usize::from(world.release)] != Some(world.generated()) {
                    return Err("said again with a foreign OUT_DIR".to_owned());
                }
            }
            Err(_) => self.walk(world, tamper, bug),
        }
        if world.lag {
            self.watches.confirm(&wanted);
        }
        Ok(())
    }

    /// The whole build: the path dependency read again unless vouched for, the kept answer
    /// dropped, the walk recorded when it is of the checkout's last plan, every unit walked
    /// and what is not in place made.
    fn walk(&mut self, world: &mut World, tamper: bool, bug: Option<Bug>) {
        if !self.dep_vouched(world) {
            self.dep_generation = self.watches.generation;
            self.dep_read = world.dep;
            if world.dep_watched {
                self.watches.want(DEP);
            }
        }
        let inputs = self.inputs(world);
        self.last = Err(Unkept::None);
        let key = Hasher::new("the model's plan").id(&inputs.view).id(&inputs.config_id).finish();
        let recording = (self.plan == Some(key)).then(|| {
            let (mut dirs, mut absent) = (vec![PathBuf::from(world.out())], Vec::new());
            match world.layout {
                true => {
                    self.watches.want(LAYOUT);
                    dirs.push(PathBuf::from(LAYOUT));
                }
                false => absent.push(PathBuf::from(LAYOUT)),
            }
            self.walked = Some(world.files());
            let mut recording =
                Recording::new(inputs, self.watches.generation, self.looks, dirs, absent);
            // The front end's word: the source, when its unit is keyed by code text; never
            // the file the build script reads.
            recording.vouch(match world.narrow {
                true => Ok(vec![StillFile {
                    path: SOURCE.to_owned(),
                    edition: ::span::Edition::CURRENT,
                }]),
                false => Err("a lint of the unit reads comments".to_owned()),
            });
            recording
        });
        if recording.is_none() {
            self.last = Err(Unkept::Planned);
        }
        self.plan = Some(key);
        self.watches.want(world.out());
        // The build script's inputs: the header is looked at on the disk unless its watch
        // vouches for the last look, and a listing always is.
        let vouched = self.header_read.is_some_and(|generation| {
            self.watches.unchanged_since(Path::new(INCLUDE), generation)
        });
        if !vouched {
            self.header_read = Some(self.watches.generation);
            self.looks += 1;
            self.watches.want(INCLUDE);
        }
        if world.lists {
            self.looks += 1;
        }
        let (answer, made_from) = (world.answer(), world.made_from());
        let profile = usize::from(world.release);
        // The build script: run again when its traced input moved, which makes something;
        // else reused, its `OUT_DIR` written again only when it is not in place as this
        // daemon left it, which no count of what a build made sees.
        let generated = Some(world.generated());
        let reran = self.script_of[profile] != Some((world.header, world.held_comment));
        let in_place = world.out_dirs[profile] == generated && self.left[profile] == generated;
        if reran || !in_place || bug == Some(Bug::ReplayWrites) {
            world.out_dirs[profile] = generated.clone();
            self.left[profile] = generated;
            self.script_of[profile] = Some((world.header, world.held_comment));
            self.watches.change(world.out());
            self.echo = true;
        }
        // The outputs are of what they are made from; a warning's quoted line is rendered
        // again by every walk and is in none of them.
        let placed = world.output().as_ref() == Some(&made_from);
        let made_nothing = !reran && placed && self.known[profile].as_ref() == Some(&made_from);
        self.known[profile] = Some(made_from.clone());
        if !placed {
            *world.output() = Some(made_from);
            self.watches.change(world.out());
        }
        if tamper {
            *world.output() = None;
            self.watches.change(world.out());
        }
        if let Some(mut recording) = recording {
            // A build that makes something says so, which one with nothing to do does not.
            let text = if made_nothing { answer } else { format!("{answer}\nCompiling") };
            recording.hear(&said(&text));
            self.last = recording.end(Walk {
                passed: true,
                made_nothing,
                looks: self.looks,
                looked_at: None,
                worked: None,
            });
        }
    }
}

/// Runs `ops` on `world` with `bug` planted: the first answer said again that a walk would
/// not say, or how many requests the lane answered.
fn runs(mut world: World, ops: &[Op], bug: Option<Bug>) -> Result<u32, String> {
    PLANTED.set(match bug {
        Some(Bug::Lane(plant)) => Some(plant),
        _ => None,
    });
    let mut daemon = Daemon::new();
    let mut ran = Ok(());
    for (at, op) in ops.iter().enumerate() {
        match *op {
            Op::Edit(input) => match input {
                Input::Source => world.source += 1,
                Input::Lock => world.lock += 1,
                Input::Header => {
                    world.header += 1;
                    daemon.watches.change(INCLUDE);
                }
                Input::Listed => world.listed += 1,
                Input::Cc => world.cc += 1,
                Input::Shlvl => world.shlvl += 1,
                Input::Features => world.features += 1,
                Input::Profile => world.release = !world.release,
                Input::Color => world.color = !world.color,
                Input::Dependency => {
                    world.dep += 1;
                    daemon.watches.change(DEP);
                }
                Input::Layout => {
                    world.layout = !world.layout;
                    daemon.watches.change(LAYOUT);
                }
                Input::Comment => world.comment += 1,
                Input::HeldComment => world.held_comment += 1,
            },
            Op::Touch => daemon.watches.change(INCLUDE),
            Op::DeleteOutput => {
                *world.output() = None;
                daemon.watches.change(world.out());
            }
            Op::EditOutDir => {
                world.out_dirs[usize::from(world.release)] = Some("foreign".to_owned());
                daemon.watches.change(world.out());
            }
            Op::Evict => daemon.known = [None, None],
            Op::OtherJob => world.other_job = !world.other_job,
            Op::Build { tamper } => {
                ran = daemon.request(&mut world, tamper, bug).map_err(|why| format!("step {at}: {why}"));
            }
        }
        if ran.is_err() {
            break;
        }
    }
    PLANTED.set(None);
    ran.map(|()| daemon.answered)
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    match tc.draw(gs::integers::<u8>().max_value(25)) {
        0 => Op::Edit(Input::Source),
        1 => Op::Edit(Input::Lock),
        2 => Op::Edit(Input::Header),
        3 => Op::Edit(Input::Listed),
        4 => Op::Edit(Input::Cc),
        5 => Op::Edit(Input::Shlvl),
        6 => Op::Edit(Input::Features),
        7 => Op::Edit(Input::Profile),
        8 => Op::Edit(Input::Color),
        9 => Op::Edit(Input::Dependency),
        10 => Op::Edit(Input::Layout),
        11 => Op::Touch,
        12 => Op::DeleteOutput,
        13 => Op::OtherJob,
        14 => Op::Build { tamper: true },
        15 => Op::Evict,
        16 => Op::EditOutDir,
        17 | 18 => Op::Edit(Input::Comment),
        19 => Op::Edit(Input::HeldComment),
        _ => Op::Build { tamper: false },
    }
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// The invariant: the lane says a kept answer again only when a walk of every unit would say
/// the same of the state the request is asked in, with every output in place and no other
/// job under way: no input a build reads changed since the answer was kept. Over any run of
/// edits of the checkout's files and its lock file, of a traced header, a listed directory,
/// a path dependency and the layout profiles outside it, of the request's environment,
/// features, profile and colors, of touches, deleted outputs, other jobs and requests, with
/// the watches confirmed in time or late; and of a `//` comment at a line's end, in the
/// source (its unit keyed by code text or, a lint reading comments, by bytes; its build
/// warning of the commented line or not) and in a file the build script reads by its bytes.
/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_kept_answer_is_said_again_only_while_a_walk_would_say_the_same() {
    run_cases(256, |tc| {
        let mut world = World::new(
            tc.draw(gs::booleans()),
            tc.draw(gs::booleans()),
            tc.draw(gs::booleans()),
        );
        world.narrow = tc.draw(gs::booleans());
        world.warns = tc.draw(gs::booleans());
        let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(40));
        if let Err(why) = runs(world.clone(), &ops, None) {
            panic!("{world:?}, {ops:?}: {why}");
        }
    });
}

const BUILD: Op = Op::Build { tamper: false };

/// A world with nothing listed, a watched path dependency and watches in place in time. Its
/// first build makes the output; its second walks, and looks at the header again, whose
/// watch was not in place when the first read it; its third walks and is kept.
fn plain() -> World {
    World::new(false, true, false)
}

/// `ops` after the three builds that leave [`plain`]'s answer kept.
fn kept_then(ops: &[Op]) -> Vec<Op> {
    [BUILD, BUILD, BUILD].into_iter().chain(ops.iter().copied()).collect()
}

/// The directed case of a control: passes with nothing planted, and with `bug` the oracle
/// says what it caught.
fn caught(world: World, ops: &[Op], bug: Bug) -> String {
    assert!(runs(world.clone(), ops, None).is_ok(), "the lane as it is must pass {ops:?}");
    runs(world, ops, Some(bug)).expect_err("the oracle missed the planted bug")
}

/// The lane is of use: once an answer is kept, each build with nothing changed is answered
/// by the lane. A variable the build never reads is in the request's environment, so its
/// change costs two walks (the plan made anew, then the walk that is kept); a touch of a
/// traced file is heard as a change and costs two (the look at it, then the walk kept).
#[test]
fn a_build_with_nothing_changed_is_answered_by_the_lane() {
    assert_eq!(runs(plain(), &kept_then(&[BUILD, BUILD]), None), Ok(2));
    let ops = kept_then(&[BUILD, Op::Edit(Input::Shlvl), BUILD, BUILD, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(2));
    let ops = kept_then(&[Op::Touch, BUILD, BUILD, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(1));
    // What the store let go of is nothing a kept answer rests on: the outputs are in place.
    assert_eq!(runs(plain(), &kept_then(&[Op::Evict, BUILD]), None), Ok(1));
}

#[test]
fn the_oracle_catches_a_lane_that_ignores_an_edited_file() {
    let ops = kept_then(&[Op::Edit(Input::Source), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresFiles)).contains("said again"));
}

#[test]
fn the_oracle_catches_a_lane_that_ignores_the_lock_file() {
    let ops = kept_then(&[Op::Edit(Input::Lock), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresFiles)).contains("said again"));
}

#[test]
fn the_oracle_catches_a_lane_that_does_not_compare_the_environment() {
    let ops = kept_then(&[Op::Edit(Input::Cc), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresConfig)).contains("said again"));
}

#[test]
fn the_oracle_catches_a_lane_that_ignores_the_features_and_the_profile() {
    for input in [Input::Features, Input::Profile] {
        let ops = kept_then(&[Op::Edit(input), BUILD]);
        assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresConfig)).contains("said again"));
    }
}

#[test]
fn the_oracle_catches_a_lane_that_ignores_how_the_request_is_rendered() {
    let ops = kept_then(&[Op::Edit(Input::Color), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresRequest)).contains("said again"));
}

#[test]
fn the_oracle_catches_a_lane_that_ignores_a_traced_file_outside_the_view() {
    let ops = kept_then(&[Op::Edit(Input::Header), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresOutside)).contains("said again"));
}

/// The output is deleted while the walk that is kept runs, before its directory's watch is
/// in place: nothing is heard of it, and only the directory's `since` says it was not
/// vouched for. The `release` profile's directory is first wanted by the build after the
/// profile's change, and with late watches is in place only as the next build ends.
#[test]
fn the_oracle_catches_a_lane_that_does_not_see_a_deleted_output() {
    let late = World::new(false, true, true);
    let ops = [
        BUILD,
        BUILD,
        BUILD,
        BUILD,
        Op::Edit(Input::Profile),
        BUILD,
        Op::Build { tamper: true },
        BUILD,
    ];
    let why = caught(late, &ops, Bug::Lane(Plant::IgnoresOutputs));
    assert!(why.contains("the output gone"), "{why}");
}

/// A walk that listed a directory looked at the disk, and no watch hears a file added there.
#[test]
fn the_oracle_catches_a_kept_walk_that_listed_a_directory() {
    let lists = World::new(true, true, false);
    let ops = [BUILD, BUILD, Op::Edit(Input::Listed), BUILD];
    assert_eq!(runs(lists.clone(), &kept_then(&[BUILD, BUILD]), None), Ok(0));
    assert!(caught(lists, &ops, Bug::Lane(Plant::KeepsALook)).contains("said again"));
}

/// A walk beside another job finds its outputs in place and makes them again (the store let
/// go of what they were made from): it says so, which a build with nothing to do does not.
#[test]
fn the_oracle_catches_a_kept_build_that_made_something() {
    let ops = kept_then(&[Op::Evict, Op::OtherJob, BUILD, Op::OtherJob, BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::KeepsWork)).contains("Compiling"));
}

#[test]
fn the_oracle_catches_a_lane_that_answers_beside_another_job() {
    let ops = kept_then(&[Op::OtherJob, BUILD]);
    let why = caught(plain(), &ops, Bug::Lane(Plant::IgnoresOthers));
    assert!(why.contains("another job"), "{why}");
}

/// A path dependency named through a link has no watch vouching for it: each build reads it
/// again, so the lane never answers; one that did would say the dependency's old answer.
#[test]
fn the_oracle_catches_a_lane_that_trusts_an_unwatched_path_dependency() {
    let linked = World::new(false, false, false);
    let ops = kept_then(&[Op::Edit(Input::Dependency), BUILD]);
    assert_eq!(runs(linked.clone(), &ops, None), Ok(0));
    assert!(caught(linked, &ops, Bug::Lane(Plant::IgnoresLocals)).contains("said again"));
}

#[test]
fn the_oracle_catches_a_lane_that_does_not_see_layout_profiles_appear() {
    let ops = kept_then(&[Op::Edit(Input::Layout), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresAppeared)).contains("said again"));
}

/// A build script reused whose `OUT_DIR` is in place writes nothing, so the walk after the
/// build that ran it is kept and the lane answers; one that wrote it again on every walk
/// would disturb each kept answer with its own echo, and the lane would never answer.
#[test]
fn the_oracle_catches_a_replayed_script_that_writes_on_every_walk() {
    let ops = kept_then(&[BUILD, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(2));
    assert_eq!(runs(plain(), &ops, Some(Bug::ReplayWrites)), Ok(0));
    // The script runs again when the header it reads changed, and is reused after.
    // The build that runs it makes something, the walk after is kept, and the two after that
    // are answered.
    let ops = kept_then(&[Op::Edit(Input::Header), BUILD, BUILD, BUILD, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(2));
}

/// A foreign write into `OUT_DIR` is heard, and the walk after it writes the script's files
/// back; a second foreign write made before that walk's own echo is heard is a change like
/// any other. A lane that took everything heard there for the echo would say the kept
/// answer over a foreign `OUT_DIR`.
#[test]
fn the_oracle_catches_a_foreign_write_taken_for_the_walks_echo() {
    let ops = kept_then(&[Op::EditOutDir, BUILD, Op::EditOutDir, BUILD]);
    let why = caught(plain(), &ops, Bug::EchoHidesForeign);
    assert!(why.contains("a foreign OUT_DIR"), "{why}");
}

/// The daemon's hold is the barrier: a request asked of the lane before the watches' round
/// is in is answered from before an edit made before it.
#[test]
fn the_oracle_catches_a_request_not_held_for_the_watches_round() {
    let ops = kept_then(&[Op::Edit(Input::Header), BUILD]);
    assert!(caught(plain(), &ops, Bug::NoRound).contains("said again"));
}

/// What `blaze why` names: the first input that moved, by its name and never its value, and
/// nothing where the last build's own work says why the next one walks.
#[test]
fn a_refusal_names_the_input() {
    let daemon = Daemon::new();
    let (mut was, mut now) = (plain(), plain());
    now.cc = 1;
    let moved = daemon.inputs(&was).moved(&daemon.inputs(&now));
    assert_eq!(moved, Some(Moved::Env("CC".to_owned())));
    assert_eq!(
        NotReplayed::Moved(Moved::Env("CC".to_owned())).input().as_deref(),
        Some("the request's variable CC")
    );
    was.source = 1;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now)), Some(Moved::Files));
    now.source = 1;
    now.cc = 0;
    now.color = true;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now)), Some(Moved::Rendering));
    now.color = false;
    now.release = true;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now)), Some(Moved::Args));
    assert_eq!(NotReplayed::NoAnswer(Unkept::Made).input(), None);
    assert_eq!(NotReplayed::NoAnswer(Unkept::None).input(), None);
    let looked = NotReplayed::NoAnswer(Unkept::Looked(Some(PathBuf::from("/net/a.h")))).input();
    assert!(looked.is_some_and(|it| it.starts_with("/net/a.h, which no watch covers")));
    let moved = NotReplayed::OutputsMoved(PathBuf::from(OUT[0])).input();
    assert_eq!(moved.as_deref(), Some("the outputs under /b/debug"));
}

/// A request that asks for its trace walks: there is a build to trace.
#[test]
fn a_traced_request_is_never_answered_by_the_lane() {
    let (daemon, world) = (Daemon::new(), plain());
    let inputs = daemon.inputs(&world);
    let recording = Recording::new(inputs.clone(), 0, 0, Vec::new(), Vec::new());
    let last = recording.end(Walk {
        passed: true,
        made_nothing: true,
        looks: 0,
        looked_at: None,
        worked: None,
    });
    let asked = |traced| Asked {
        inputs: &inputs,
        edited: Edited::Unread("the roots are one"),
        traced,
        alone: true,
        locals_vouched: true,
    };
    assert!(ask(&last, &asked(false), &daemon.watches).is_ok());
    assert_eq!(ask(&last, &asked(true), &daemon.watches).err(), Some(NotReplayed::Traced));
}

/// An edit of what no code text holds ends at the file: a `//` comment at a line's end of a
/// source keyed by code text is answered by the lane, again and again, and so is the build
/// after it with nothing changed. A token of the same file walks.
#[test]
fn a_comment_edit_is_answered_by_the_lane() {
    let comment = Op::Edit(Input::Comment);
    assert_eq!(runs(plain(), &kept_then(&[comment, BUILD, BUILD]), None), Ok(2));
    assert_eq!(runs(plain(), &kept_then(&[comment, BUILD, comment, comment, BUILD]), None), Ok(2));
    assert_eq!(runs(plain(), &kept_then(&[Op::Edit(Input::Source), BUILD]), None), Ok(0));
    // A comment edit beside a token's: the file's still text moved.
    let ops = kept_then(&[comment, Op::Edit(Input::Source), BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(0));
    // Beside the lock file's: a file that is no source.
    assert_eq!(runs(plain(), &kept_then(&[comment, Op::Edit(Input::Lock), BUILD]), None), Ok(0));
    // And with the request's features changed, the files as they were to every key: the
    // config is compared whatever the files are.
    let ops = kept_then(&[comment, Op::Edit(Input::Features), BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(0));
}

/// What does not qualify walks: a comment in a file the build script reads by its bytes
/// (the script runs again), one in a source a lint reads the comments of (its unit is keyed
/// by its bytes), and one a warning of the kept answer quotes (the walk renders it again).
#[test]
fn a_comment_edit_something_reads_walks() {
    let held = kept_then(&[Op::Edit(Input::HeldComment), BUILD]);
    assert_eq!(runs(plain(), &held, None), Ok(0));
    let comment = kept_then(&[Op::Edit(Input::Comment), BUILD]);
    let linted = World { narrow: false, ..plain() };
    assert_eq!(runs(linted, &comment, None), Ok(0));
    let warned = World { warns: true, ..plain() };
    assert_eq!(runs(warned.clone(), &comment, None), Ok(0));
    // The warning is said again while nothing changed, as any answer is.
    assert_eq!(runs(warned, &kept_then(&[BUILD]), None), Ok(1));
}

/// A lane keyed by code text that takes any change of a source for a comment's says the
/// last answer over a body edit.
#[test]
fn the_oracle_catches_a_lane_keyed_by_code_text_that_misses_a_token_change() {
    let ops = kept_then(&[Op::Edit(Input::Source), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresTokens)).contains("said again"));
}

/// A lane that takes a file of one still text for a source held by code text says the last
/// answer over a comment in a file the build script reads, and one a lint reads.
#[test]
fn the_oracle_catches_a_lane_that_takes_bytes_something_reads_for_still() {
    let ops = kept_then(&[Op::Edit(Input::HeldComment), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresHeld)).contains("said again"));
    // A lint's: the unit's own front end vouched for no source, which a lane that held
    // every file would not ask. The plant is of the file's place in the list, so the
    // refusal here is the list's absence.
    let linted = World { narrow: false, ..plain() };
    let ops = kept_then(&[Op::Edit(Input::Comment), BUILD]);
    assert_eq!(runs(linted, &ops, Some(Bug::Lane(Plant::IgnoresHeld))), Ok(0));
}

/// A lane that says an answer again over a changed file its diagnostics quote says the
/// quoted line with the comment the file no longer has.
#[test]
fn the_oracle_catches_a_replayed_diagnostic_with_stale_quoted_text() {
    let warned = World { warns: true, ..plain() };
    let ops = kept_then(&[Op::Edit(Input::Comment), BUILD]);
    let why = caught(warned, &ops, Bug::Lane(Plant::ReplaysQuotes));
    assert!(why.contains("said again") && why.contains("code //"), "{why}");
}

/// What `blaze why` names of a file that changed: the file, where the lane read which.
#[test]
fn a_refusal_names_the_file() {
    let moved = NotReplayed::Moved(Moved::File(LOCK.to_owned()));
    assert_eq!(moved.input().as_deref(), Some("the checkout's file Cargo.lock"));
    let quoted = NotReplayed::Quoted(SOURCE.to_owned()).input();
    assert!(quoted.is_some_and(|it| it.contains("src/lib.rs")));
}
