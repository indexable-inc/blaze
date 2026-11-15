//! The no-op lane against a walk: a model checkout, its disk and its builds, driven by any run
//! of edits and requests of several shapes (a build or a test build, in either profile). The
//! lane's own code decides ([`Answers`], [`ask`], [`Recording`]); the model is what a daemon
//! gives it (the view's root, the config, each stamp it asks for, which variables a step
//! reads) and what a walk of every unit would say.

use std::{cell::Cell, collections::BTreeMap};

use blaze_api::{Diagnostic, Level};
use blaze_sim::{Config, Tape};
use blaze_units::query::{Command, Invocation, Mount, Origin};

use super::*;
use blaze_vfs::outside::Look;

/// A look at `path` of `kind` that said `answer` under `stamp`, taken at `looked_ns`, made
/// here.
#[track_caller]
fn look(path: &str, kind: LookKind, answer: Option<[u8; 32]>, stamp: Stamp, looked_ns: u64) -> Look {
    Look { path: PathBuf::from(path), kind, answer, stamp, looked_ns, by: std::panic::Location::caller() }
}

/// The directories of the checkout's outputs, the `dev` profile's and `release`'s.
const OUT: [&str; 2] = ["/b/debug", "/b/release"];
/// A directory outside the view, which the build script lists when it lists one.
const INCLUDE: &str = "/usr/include";
/// The header the build script reads, under [`INCLUDE`].
const HEADER: &str = "/usr/include/a.h";
/// The path dependency outside the view, and its one file.
const DEP: &str = "/dep";
const DEP_FILE: &str = "/dep/src/lib.rs";
/// The directory of layout profiles, which is there or not.
const LAYOUT: &str = "/b/layout";
/// The checkout's source, a file of its one crate, by its path from the view's root.
const SOURCE: &str = "src/lib.rs";
/// A Rust file of the checkout the build script reads by its bytes.
const HELD: &str = "gen/input.rs";
/// The lock file.
const LOCK: &str = "Cargo.lock";

/// The model's file system clock as it starts, and how far it moves at each step of a run:
/// neither a whole second, so the racy window is the fine one ([`blaze_vfs::FINE_NS`]), and a
/// step is far past it. A write in the step a look is made in is racy by that look.
const BASE_NS: u64 = 1_700_000_000_000_000_007;
const TICK_NS: u64 = 1_000_000_007;
/// The modes of a file and a directory.
const FILE: u32 = 0o100644;
const DIR: u32 = 0o040755;

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
    /// The header outside the view the build script reads ([`HEADER`]).
    header: u8,
    /// Whether the build script lists [`INCLUDE`], and how many files it holds.
    lists: bool,
    listed: u8,
    /// A variable the build reads without asking for it (the C compiler a build script's
    /// walk of its environment sees), one the build script asks for by name, and one nothing
    /// reads.
    cc: u8,
    made: u8,
    shlvl: u8,
    /// Whether the walk's steps can say which variables they asked for by name and which
    /// files under the outputs they read (not when a build script's run was not found kept
    /// as the build began).
    names: bool,
    /// Whether the request is a test build: another shape of request, whose output is its
    /// own file in the same directory.
    test: bool,
    /// Whether the checkout has its lock file: a build of one without it writes it.
    lock_there: bool,
    features: u8,
    release: bool,
    /// Whether the request's diagnostics are colored.
    color: bool,
    /// The path dependency's content, and whether it is named plainly (not through a link,
    /// which keeps no leaves, so it is read again by every build).
    dep: u8,
    dep_plain: bool,
    /// Whether the layout profiles' directory is there.
    layout: bool,
    /// What each kind's output file of each profile holds, when it is in place: the answer it
    /// was made from. A build's first, then a test build's.
    outputs: [[Option<String>; 2]; 2],
    /// What each profile's build script's `OUT_DIR` holds, under the profile's directory of
    /// outputs: what the script made of the header, or what a foreign process wrote there.
    out_dirs: [Option<String>; 2],
    /// Another job is under way, which may write the outputs at any time.
    other_job: bool,
}

impl World {
    fn new(lists: bool, dep_plain: bool) -> World {
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
            made: 0,
            shlvl: 0,
            names: true,
            test: false,
            lock_there: true,
            features: 0,
            release: false,
            color: false,
            dep: 0,
            dep_plain,
            layout: false,
            outputs: [[None, None], [None, None]],
            out_dirs: [None, None],
            other_job: false,
        }
    }

    /// Where the profile asked for places its output.
    fn out(&self) -> &'static str {
        OUT[usize::from(self.release)]
    }

    /// Where the output of the kind and profile asked for is placed: a build's and a test
    /// build's are two files of one directory.
    fn output_path(&self) -> String {
        format!("{}/{}", self.out(), if self.test { "app-test" } else { "app" })
    }

    /// The build script's `OUT_DIR` of the profile asked for: under the directory of outputs,
    /// and no planned output. The model holds it as one file.
    fn out_dir_path(&self) -> String {
        format!("{}/out", self.out())
    }

    /// The output of the kind and profile asked for.
    fn output(&mut self) -> &mut Option<String> {
        &mut self.outputs[usize::from(self.test)][usize::from(self.release)]
    }

    /// What the build script makes in its `OUT_DIR`: a function of the header it reads, of
    /// the bytes of [`HELD`] and of the variable it asks for by name.
    fn generated(&self) -> String {
        format!(
            "generated from header {} and input {} and variable {}",
            self.header, self.held_comment, self.made
        )
    }

    /// What the build's outputs are made from: every input a build reads, and none it does
    /// not (`shlvl`; the source's comment, for a unit keyed by code text; how the client is
    /// shown diagnostics, `color`).
    fn made_from(&self) -> String {
        let listed = if self.lists { self.listed } else { 0 };
        // A lint that reads comments reads this one.
        let read = if self.narrow { 0 } else { self.comment };
        format!(
            "test {} source {} read {read} input {} lock {} header {} listed {listed} cc {} made {} features {} release {} dep {} layout {}",
            self.test,
            self.source,
            self.held_comment,
            self.lock,
            self.header,
            self.cc,
            self.made,
            self.features,
            self.release,
            self.dep,
            self.layout,
        )
    }

    /// What a walk of every unit says of this state, with nothing to make: what its outputs
    /// are made from and, when it warns, its warning ([`World::warning`]).
    fn answer(&self) -> String {
        match self.warns {
            true => format!("{}\n{}", self.made_from(), self.warning()),
            false => self.made_from(),
        }
    }

    /// The build's warning as its client is shown it: the source's line as it is now, comment
    /// and all, under the file's name, drawn in the client's colour.
    fn warning(&self) -> String {
        format!(" --> {SOURCE} | code // {} color {}", self.comment, self.color)
    }

    /// The checkout's files as a view holds them: what its root is of. The lock file's
    /// content is what the plan resolves, there or not; the last says whether the file is.
    fn files(&self) -> [u8; 5] {
        [self.source, self.comment, self.held_comment, self.lock, u8::from(self.lock_there)]
    }
}

/// The still text of a Rust file whose tokens are `tokens`: nothing of its comments.
fn still_of(tokens: u8) -> ContentId {
    id(&format!("still {tokens}"))
}

/// The files that differ between two views of the checkout, as the daemon reads them: each
/// Rust one with its still texts, the lock file with none.
fn edits(was: [u8; 5], now: [u8; 5]) -> Vec<FileEdit> {
    let mut edits = Vec::new();
    if was[3] != now[3] || was[4] != now[4] {
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

/// The disk outside the view as the kernel stamps it: each path's stamp, by a clock that
/// moves a step at a time. A write in place keeps the inode and sets both times to now, so two
/// writes in one step leave one stamp (the racy case); a placement or a directory made gets
/// another inode. Counts every stamp asked of it.
struct Disk {
    clock: u64,
    stamps: BTreeMap<String, Stamp>,
    inos: u64,
    asked: Cell<u64>,
}

impl Disk {
    fn new() -> Disk {
        let mut disk = Disk { clock: BASE_NS, stamps: BTreeMap::new(), inos: 0, asked: Cell::new(0) };
        disk.put(INCLUDE, DIR, true);
        disk.write(HEADER);
        disk.write(DEP_FILE);
        disk
    }

    fn tick(&mut self) {
        self.clock += TICK_NS;
    }

    fn at(&self, path: &str) -> Stamp {
        self.stamps.get(path).copied().unwrap_or(Stamp::GONE)
    }

    /// The file at `path` written in place now.
    fn write(&mut self, path: &str) {
        self.put(path, FILE, false);
    }

    /// The file at `path` placed anew now (a rename onto it).
    fn replace(&mut self, path: &str) {
        self.put(path, FILE, true);
    }

    /// An entry made in the directory at `path` now: its times move, its inode stays.
    fn entry_made(&mut self, path: &str) {
        self.put(path, DIR, false);
    }

    fn put(&mut self, path: &str, mode: u32, anew: bool) {
        let was = self.at(path);
        let ino = match anew || was.is_gone() {
            true => {
                self.inos += 1;
                self.inos
            }
            false => was.ino,
        };
        let clock = self.clock;
        self.stamps.insert(path.to_owned(), Stamp { ino, size: 1, mtime_ns: clock, ctime_ns: clock, mode });
    }

    fn remove(&mut self, path: &str) {
        self.stamps.insert(path.to_owned(), Stamp::GONE);
    }
}

impl Stamps for Disk {
    fn stamp(&self, path: &Path, _: LookKind) -> Stamp {
        self.asked.set(self.asked.get() + 1);
        self.at(path.to_str().expect("the model's paths are UTF-8"))
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
    /// The variable the build script asks for by name.
    Made,
    Shlvl,
    Features,
    Profile,
    /// The request is a test build, or a build again: another shape.
    Kind,
    Color,
    Dependency,
    Layout,
    /// The `//` comment at the end of a line of the source says something else.
    Comment,
    /// The `//` comment in the file the build script reads says something else.
    HeldComment,
}

/// A step of a run. Each moves the clock a step but [`Op::RacyBuild`].
#[derive(Clone, Copy, Debug)]
enum Op {
    /// The input gets another value.
    Edit(Input),
    /// The header is written with the bytes it has: its stamp moves, its answer does not.
    Touch,
    DeleteOutput,
    /// Another process writes over the output (a stale copy, a build by another tool).
    ReplaceOutput,
    /// A foreign process writes into the build script's `OUT_DIR`.
    EditOutDir,
    /// The store lets go of what the outputs were made from: the next walk makes them
    /// again, and finds the files in place as it would place them.
    Evict,
    /// Another job starts, or the one under way ends.
    OtherJob,
    /// The lock file is removed from the checkout: the next build writes it again.
    RemoveLock,
    /// A request. With `tamper`, the output is deleted while its walk runs, after the walk
    /// stamped it.
    Build { tamper: bool },
    /// In the step of the one before: the header is written with the bytes it has, a request
    /// is made, and the header is written again with other bytes, which leaves its stamp as
    /// the request's walk saw it (the racy case).
    RacyBuild,
}

/// A bug planted: one of the lane's ([`Plant`]), or the model daemon's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bug {
    Lane(Plant),
    /// A build script reused writes its `OUT_DIR` again on every walk, in place or not.
    ReplayWrites,
    /// A build that writes the checkout's lock file records the view it was asked on, which
    /// lacks the file, as the daemon did until 2026-11-12: the file comes back from the
    /// watcher as a change, and the next build plans anew on a view no build has seen.
    EchoIsAChange,
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

/// What a build says of what its outputs are made from: no diagnostic, and drawn for no
/// client.
fn built(text: &str) -> Response {
    Response::Built { members: 1, profile: text.to_owned(), artifacts: Vec::new() }
}

fn text_of(responses: &[Response]) -> String {
    let texts = responses.iter().map(|it| match it {
        Response::Diagnostic(diagnostic) => diagnostic.rendered.as_str(),
        Response::Built { profile, .. } => profile.as_str(),
        _ => "a response the model never says",
    });
    texts.collect::<Vec<_>>().join("\n")
}

/// The model's daemon: the lane's state for the checkout, and what a daemon holds beside it.
struct Daemon {
    /// The checkout's answers, one a request shape, each with the files of the view its walk
    /// read: what the daemon holds a view of, to read what differs from it.
    answers: Answers<[u8; 5]>,
    /// The key of the checkout's plan of each shape: its view and config.
    plans: BTreeMap<ContentId, ContentId>,
    /// How many walks planned anew.
    planned: u32,
    /// What the store holds each kind's output of each profile made from.
    known: [[Option<String>; 2]; 2],
    disk: Disk,
    /// The path dependency as it was last read, and its file's stamp then with the instant
    /// it was taken (none when it is named through a link: no leaves).
    dep_read: u8,
    dep_leaf: Option<(Stamp, u64)>,
    /// The header, the input and the named variable each profile's kept script run is of, and
    /// what this daemon left in each `OUT_DIR`.
    script_of: [Option<(u8, u8, u8)>; 2],
    left: [Option<String>; 2],
    /// How many requests the lane answered.
    answered: u32,
}

/// Whether a step of the model's build reads the variable `name` without asking for it by
/// name: the C compiler's, which a build script's walk of its environment sees.
fn unasked(name: &str) -> bool {
    name == "CC"
}

impl Daemon {
    fn new() -> Daemon {
        Daemon {
            answers: Answers::new(),
            plans: BTreeMap::new(),
            planned: 0,
            known: [[None, None], [None, None]],
            disk: Disk::new(),
            dep_read: 0,
            dep_leaf: None,
            script_of: [None, None],
            left: [None, None],
            answered: 0,
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
                command: if world.test { Command::Test } else { Command::Build },
                args,
                root: "/w".to_owned(),
                cwd: "/w".to_owned(),
                env: vec![
                    ("CC".to_owned(), world.cc.to_string()),
                    ("MADE".to_owned(), world.made.to_string()),
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
        // The config less its environment, as the daemon hashes it: of the environment the
        // lane compares what was read. The model's plan reads no variable, so this is also
        // the request's shape.
        let mut bare = config.clone();
        bare.invocation.env.clear();
        let config_id = Hasher::new("the no-op lane's test config").part(&bare.encode()).finish();
        Inputs {
            view: id(&format!("view {:?}", world.files())),
            config: Arc::new(config),
            config_id,
            request: id("request"),
            drawn: id(&format!("drawn {}", world.color)),
        }
    }

    /// Whether the path dependency's leaf holds since it was read (`Sweep::stale`).
    fn dep_vouched(&self) -> bool {
        self.dep_leaf.is_some_and(|(was, at)| blaze_vfs::outside::holds(&was, at, &self.disk.at(DEP_FILE)))
    }

    /// A request: asked of the lane, and walked when the lane refuses. The oracle is asked of
    /// an answer said again.
    fn request(&mut self, world: &mut World, tamper: bool, bug: Option<Bug>) -> Result<(), String> {
        let inputs = self.inputs(world);
        let shape = shape(&inputs);
        // What the daemon reads of the files, only where the answer's view is another root.
        let read = match self.answers.held_mut(&shape) {
            Some((answer, was)) if !answer.files_known(&inputs.view) => {
                Some(edits(*was, world.files()))
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
            locals_vouched: self.dep_vouched(),
            unasked: &unasked,
        };
        match self.answers.ask(&shape, &asked, &self.disk) {
            Ok((responses, _)) => {
                self.answered += 1;
                let (again, walk) = (text_of(&responses), world.answer());
                if again != walk {
                    return Err(format!("said again: `{again}`, where a walk says `{walk}`"));
                }
                let made_from = world.made_from();
                match world.output().as_ref() {
                    None => return Err("said again with the output gone".to_owned()),
                    Some(output) if *output != made_from => {
                        return Err(format!("said again with an output another wrote: `{output}`"));
                    }
                    Some(_) => {}
                }
                if world.other_job {
                    return Err("said again while another job may write the outputs".to_owned());
                }
                if !world.lock_there {
                    return Err("said again with the lock file gone, which a walk writes".to_owned());
                }
                if world.out_dirs[usize::from(world.release)] != Some(world.generated()) {
                    return Err("said again with a foreign OUT_DIR".to_owned());
                }
            }
            Err(_) => self.walk(world, tamper, bug),
        }
        Ok(())
    }

    /// The whole build: the path dependency read again unless its leaf holds, the lock file
    /// written into the view where the checkout lacks it, the shape's kept answer dropped
    /// (and no other shape's), the walk recorded when it is of the shape's plan, every unit
    /// walked and what is not in place made.
    fn walk(&mut self, world: &mut World, tamper: bool, bug: Option<Bug>) {
        let looked_ns = self.disk.clock;
        if !self.dep_vouched() {
            self.dep_read = world.dep;
            self.dep_leaf = world.dep_plain.then(|| (self.disk.at(DEP_FILE), looked_ns));
        }
        // A checkout without its lock file: the plan writes it, through the view, so the
        // view this build is of holds it and the file the watcher reports later is the one
        // the view has. The bug records the view the build was asked on.
        let asked_on = (self.inputs(world), world.files());
        world.lock_there = true;
        let (inputs, files) = match bug {
            Some(Bug::EchoIsAChange) => asked_on,
            _ => (self.inputs(world), world.files()),
        };
        let shape = shape(&inputs);
        self.answers.begin(&shape);
        let key = Hasher::new("the model's plan").id(&inputs.view).id(&inputs.config_id).finish();
        let planned = self.plans.get(&shape) != Some(&key);
        self.planned += u32::from(planned);
        let disk = &self.disk;
        let recording = (!planned).then(|| {
            // The outputs and the layout profiles' directory, stamped before the walk looks
            // at anything (`job::recorded_dirs`).
            let stamped = |path: String, kind| Stamped { stamp: disk.at(&path), path: PathBuf::from(path), kind, looked_ns };
            let outputs = [stamped(world.output_path(), LookKind::Entry), stamped(LAYOUT.to_owned(), LookKind::Listing)];
            let mut recording = Recording::new(inputs, looked_ns, vec![PathBuf::from(world.out())], outputs);
            // The steps' word: the variable the build script asked for by name, when its
            // kept run's reads are known.
            recording.vouch_env(match world.names {
                true => Ok(vec!["MADE".to_owned()]),
                false => Err("a build script's run was not found kept".to_owned()),
            });
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
            self.answers.keep(shape, Err(Unkept::Planned), None);
        }
        self.plans.insert(shape, key);
        // This walk's own looks at the disk ([`blaze_vfs::outside::Looks`]), each with its
        // stamp: the header a file's, the listing a directory's.
        let mut looks = vec![look(HEADER, LookKind::File, Some([world.header; 32]), self.disk.at(HEADER), looked_ns)];
        if world.lists {
            looks.push(look(INCLUDE, LookKind::Listing, Some([world.listed; 32]), self.disk.at(INCLUDE), looked_ns));
        }
        let made_from = world.made_from();
        let profile = usize::from(world.release);
        // The build script: run again when its traced input moved, which makes something;
        // else reused, its `OUT_DIR` written again only when it is not in place as this
        // daemon left it, which no count of what a build made sees.
        let generated = Some(world.generated());
        let ran_of = (world.header, world.held_comment, world.made);
        let reran = self.script_of[profile] != Some(ran_of);
        let in_place = world.out_dirs[profile] == generated && self.left[profile] == generated;
        if reran || !in_place || bug == Some(Bug::ReplayWrites) {
            world.out_dirs[profile] = generated.clone();
            self.left[profile] = generated;
            self.script_of[profile] = Some(ran_of);
            self.disk.write(&world.out_dir_path());
        }
        // The outputs are of what they are made from; a warning's quoted line is rendered
        // again by every walk and is in none of them.
        let placed = world.output().as_ref() == Some(&made_from);
        let kind = usize::from(world.test);
        let made_nothing = !reran && placed && self.known[kind][profile].as_ref() == Some(&made_from);
        self.known[kind][profile] = Some(made_from.clone());
        let output_path = world.output_path();
        if !placed {
            *world.output() = Some(made_from);
            self.disk.replace(&output_path);
        }
        if tamper {
            *world.output() = None;
            self.disk.remove(&output_path);
        }
        if let Some(mut recording) = recording {
            // And the paths the steps read, as they say once the walk ended: the header, and
            // the build script's `OUT_DIR`, under the directory of outputs, stamped now.
            let reads = [PathBuf::from(HEADER), PathBuf::from(world.out_dir_path())];
            let listed: &[PathBuf] = &[];
            let read = match world.names {
                true => Ok((reads.as_slice(), listed)),
                false => Err("a build script's run was not found kept"),
            };
            recording.vouch_reads(read, &self.disk);
            // A build that makes something says so, which one with nothing to do does not.
            // What its outputs are made from is said to every client alike; its warning is a
            // diagnostic, drawn for this one.
            let state = world.made_from();
            let state = if made_nothing { state } else { format!("{state}\nCompiling") };
            recording.hear(&built(&state));
            if world.warns {
                recording.hear(&said(&world.warning()));
            }
            let last = recording.end(Walk {
                passed: true,
                made_nothing,
                looks,
                worked: None,
            });
            self.answers.keep(shape, last, Some(files));
        }
    }
}

/// The request's shape as the daemon keys its answers and plans by it (`job::shape_of`), of
/// the model's plan's config: the config less its environment, as the model's plan reads no
/// variable.
fn shape(inputs: &Inputs) -> ContentId {
    super::super::shape_of(&inputs.config_id.0)
}

/// Runs `ops` on `world` with `bug` planted: the first answer said again that a walk would
/// not say, or how many requests the lane answered.
fn runs(world: World, ops: &[Op], bug: Option<Bug>) -> Result<u32, String> {
    counts(world, ops, bug).map(|(answered, _)| answered)
}

/// As [`runs`], with how many walks planned anew.
fn counts(mut world: World, ops: &[Op], bug: Option<Bug>) -> Result<(u32, u32), String> {
    PLANTED.set(match bug {
        Some(Bug::Lane(plant)) => Some(plant),
        _ => None,
    });
    let mut daemon = Daemon::new();
    let mut ran = Ok(());
    for (at, op) in ops.iter().enumerate() {
        if !matches!(op, Op::RacyBuild) {
            daemon.disk.tick();
        }
        match *op {
            Op::Edit(input) => match input {
                Input::Source => world.source += 1,
                Input::Lock => world.lock += 1,
                Input::Header => {
                    world.header += 1;
                    daemon.disk.write(HEADER);
                }
                Input::Listed => {
                    world.listed += 1;
                    daemon.disk.entry_made(INCLUDE);
                }
                Input::Cc => world.cc += 1,
                Input::Made => world.made += 1,
                Input::Shlvl => world.shlvl += 1,
                Input::Features => world.features += 1,
                Input::Profile => world.release = !world.release,
                Input::Kind => world.test = !world.test,
                Input::Color => world.color = !world.color,
                Input::Dependency => {
                    world.dep += 1;
                    daemon.disk.write(DEP_FILE);
                }
                Input::Layout => {
                    world.layout = !world.layout;
                    match world.layout {
                        true => daemon.disk.put(LAYOUT, DIR, true),
                        false => daemon.disk.remove(LAYOUT),
                    }
                }
                Input::Comment => world.comment += 1,
                Input::HeldComment => world.held_comment += 1,
            },
            Op::Touch => daemon.disk.write(HEADER),
            Op::DeleteOutput => {
                *world.output() = None;
                daemon.disk.remove(&world.output_path());
            }
            Op::ReplaceOutput => {
                *world.output() = Some("foreign".to_owned());
                daemon.disk.replace(&world.output_path());
            }
            Op::EditOutDir => {
                world.out_dirs[usize::from(world.release)] = Some("foreign".to_owned());
                daemon.disk.write(&world.out_dir_path());
            }
            Op::Evict => daemon.known = [[None, None], [None, None]],
            Op::OtherJob => world.other_job = !world.other_job,
            Op::RemoveLock => world.lock_there = false,
            Op::Build { tamper } => {
                ran = daemon.request(&mut world, tamper, bug).map_err(|why| format!("step {at}: {why}"));
            }
            Op::RacyBuild => {
                daemon.disk.write(HEADER);
                ran = daemon.request(&mut world, false, bug).map_err(|why| format!("step {at}: {why}"));
                world.header += 1;
                daemon.disk.write(HEADER);
            }
        }
        if ran.is_err() {
            break;
        }
    }
    PLANTED.set(None);
    ran.map(|()| (daemon.answered, daemon.planned))
}

fn op(tape: &Tape) -> Op {
    match tape.int_in(0..=33u8, "op") {
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
        20 | 21 => Op::Edit(Input::Kind),
        22 => Op::Edit(Input::Made),
        23 => Op::RemoveLock,
        24 => Op::ReplaceOutput,
        25 => Op::RacyBuild,
        _ => Op::Build { tamper: false },
    }
}

/// The invariant: the lane says a kept answer again only when a walk of every unit would say
/// the same of the state the request is asked in, with every output in place as the walk
/// left it and no other job under way: no input a build reads changed since the answer was
/// kept, whatever requests of other shapes were walked or answered between (an answer is
/// kept for each shape, and a request is answered from its own shape's alone). Over any run
/// of edits of the checkout's files and its lock file (removed, and written again by the
/// build that finds it gone), of a traced header, a listed directory, a path dependency and
/// the layout profiles outside it, of the request's environment (a variable a step reads
/// unasked, one a build script asks for by name, one nothing reads; the names known or not),
/// of its kind (a build or a test build), features, profile and colors, of touches, deleted
/// and replaced outputs, writes in the tick a walk looked (racy stamps), other jobs and
/// requests; and of a `//` comment at a line's end, in the source (its unit keyed by code
/// text or, a lint reading comments, by bytes; its build warning of the commented line or
/// not) and in a file the build script reads by its bytes. `BLAZE_SIM_SEEDS=<n>` runs it
/// longer.
#[test]
fn a_kept_answer_is_said_again_only_while_a_walk_would_say_the_same() {
    blaze_sim::check(Config::cases(256), |tape| {
        let mut world = World::new(tape.bool("lists"), tape.bool("dep plain"));
        world.narrow = tape.bool("narrow");
        world.warns = tape.bool("warns");
        world.names = tape.bool("names");
        world.lock_there = tape.bool("lock there");
        let ops = tape.vec(1..=40, "ops", op);
        if let Err(why) = runs(world.clone(), &ops, None) {
            panic!("{world:?}, {ops:?}: {why}");
        }
    });
}

const BUILD: Op = Op::Build { tamper: false };

/// A world with nothing listed and a path dependency named plainly. Its first build makes
/// the output; its second walks and is kept.
fn plain() -> World {
    World::new(false, true)
}

/// `ops` after the two builds that leave [`plain`]'s answer kept.
fn kept_then(ops: &[Op]) -> Vec<Op> {
    [BUILD, BUILD].into_iter().chain(ops.iter().copied()).collect()
}

/// The directed case of a control: passes with nothing planted, and with `bug` the oracle
/// says what it caught.
fn caught(world: World, ops: &[Op], bug: Bug) -> String {
    assert!(runs(world.clone(), ops, None).is_ok(), "the lane as it is must pass {ops:?}");
    runs(world, ops, Some(bug)).expect_err("the oracle missed the planted bug")
}

/// The lane is of use: once an answer is kept, each build with nothing changed is answered
/// by the lane. A variable the build never reads is no input of the answer, so a build
/// after its change is answered as any other; a touch of a traced file moves its stamp and
/// costs one walk, which is kept.
#[test]
fn a_build_with_nothing_changed_is_answered_by_the_lane() {
    assert_eq!(runs(plain(), &kept_then(&[BUILD, BUILD]), None), Ok(2));
    let ops = kept_then(&[BUILD, Op::Edit(Input::Shlvl), BUILD, BUILD, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(4));
    let ops = kept_then(&[Op::Touch, BUILD, BUILD, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(2));
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

/// A request of another shape (other features, the other profile, a test build) has no
/// answer of its own yet: one answered from the answer of the shape asked before is said
/// what that other request's walk said.
#[test]
fn the_oracle_catches_a_lane_that_ignores_the_features_and_the_profile() {
    for input in [Input::Features, Input::Profile, Input::Kind] {
        let ops = kept_then(&[Op::Edit(input), BUILD]);
        assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresConfig)).contains("said again"));
    }
}

#[test]
fn the_oracle_catches_a_lane_that_ignores_how_the_request_is_rendered() {
    let ops = kept_then(&[Op::Edit(Input::Color), BUILD]);
    let warns = World { warns: true, ..plain() };
    assert!(caught(warns, &ops, Bug::Lane(Plant::IgnoresRequest)).contains("said again"));
}

/// How a client is shown diagnostics is no input of what is built, and of an answer only its
/// diagnostics are drawn with it: a build that said none is answered again for a client
/// shown them another way (a terminal of another width, in colour), and one that warned
/// walks, which draws its warning for that client.
#[test]
fn an_answer_with_no_diagnostic_is_said_again_to_a_client_shown_them_another_way() {
    let ops = kept_then(&[Op::Edit(Input::Color), BUILD, Op::Edit(Input::Color), BUILD]);
    let same = kept_then(&[BUILD, BUILD]);
    let quiet = runs(plain(), &ops, None).unwrap();
    assert_eq!(quiet, runs(plain(), &same, None).unwrap(), "every build after the kept one");
    assert_eq!(quiet, 2);
    let warns = World { warns: true, ..plain() };
    assert_eq!(runs(warns.clone(), &same, None).unwrap(), 2);
    assert_eq!(runs(warns, &ops, None).unwrap(), 0, "each one walks, and draws the warning again");
}

/// A leaf whose stamp moved is replayed: the header a build script read is written, and a
/// lane that swept no leaf says the answer of the header before.
#[test]
fn the_oracle_catches_a_leaf_whose_stamp_moved_replayed() {
    let ops = kept_then(&[Op::Edit(Input::Header), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresLeaves)).contains("said again"));
}

/// A replaced output keeps the answer: another process writes over the output, deletes it,
/// or deletes it while the walk that is kept runs (after the walk stamped it), or writes into
/// the build script's `OUT_DIR`. Each moves a stamp of the answer's outputs, so the lane
/// walks; one that swept none says the answer over what is not the walk's.
#[test]
fn the_oracle_catches_a_replaced_output_that_keeps_the_answer() {
    let bug = Bug::Lane(Plant::IgnoresOutputs);
    let why = caught(plain(), &kept_then(&[Op::ReplaceOutput, BUILD]), bug);
    assert!(why.contains("an output another wrote"), "{why}");
    let why = caught(plain(), &kept_then(&[Op::DeleteOutput, BUILD]), bug);
    assert!(why.contains("the output gone"), "{why}");
    let tamper = kept_then(&[Op::Edit(Input::Header), BUILD, Op::Build { tamper: true }, BUILD]);
    let why = caught(plain(), &tamper, bug);
    assert!(why.contains("the output gone"), "{why}");
    let why = caught(plain(), &kept_then(&[Op::EditOutDir, BUILD]), bug);
    assert!(why.contains("a foreign OUT_DIR"), "{why}");
    // A test build's output placed beside the build's moves none of the build's stamps, and
    // the build's own output deleted after it still does.
    let kind = Op::Edit(Input::Kind);
    let ops = kept_then(&[kind, BUILD, kind, Op::DeleteOutput, BUILD]);
    assert!(caught(plain(), &ops, bug).contains("the output gone"));
}

/// A racy stamp is trusted: the header is written in the tick a kept walk looks at it, and
/// again in that tick after the walk with other bytes, which leaves its stamp as the walk saw
/// it. The lane walks the build after, as the racy rule says; one that trusted the stamp
/// says the answer of the header before.
#[test]
fn the_oracle_catches_a_racy_stamp_trusted() {
    let ops = kept_then(&[BUILD, Op::RacyBuild, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(1), "the build before the racy one, none after");
    let why = caught(plain(), &ops, Bug::Lane(Plant::TrustsRacy));
    assert!(why.contains("said again"), "{why}");
}

/// A walk that listed a directory outside the view keeps the directory's stamp, so it is
/// answered until an entry is made there, which moves it.
#[test]
fn a_walk_that_listed_a_directory_is_answered_until_an_entry_is_made() {
    let lists = World::new(true, true);
    assert_eq!(runs(lists.clone(), &kept_then(&[BUILD, BUILD]), None), Ok(2));
    let ops = kept_then(&[Op::Edit(Input::Listed), BUILD]);
    assert_eq!(runs(lists.clone(), &ops, None), Ok(0));
    assert!(caught(lists, &ops, Bug::Lane(Plant::IgnoresLeaves)).contains("said again"));
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

/// A path dependency named through a link has no leaves: each build reads it again, so the
/// lane never answers; one that did would say the dependency's old answer.
#[test]
fn the_oracle_catches_a_lane_that_trusts_a_path_dependency_with_no_leaves() {
    let linked = World::new(false, false);
    let ops = kept_then(&[Op::Edit(Input::Dependency), BUILD]);
    assert_eq!(runs(linked.clone(), &ops, None), Ok(0));
    assert!(caught(linked, &ops, Bug::Lane(Plant::IgnoresLocals)).contains("said again"));
}

/// The layout profiles' directory is stamped with the outputs, [`Stamp::GONE`] while it is
/// not there: made, it moves.
#[test]
fn the_oracle_catches_a_lane_that_does_not_see_layout_profiles_appear() {
    let ops = kept_then(&[Op::Edit(Input::Layout), BUILD]);
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresOutputs)).contains("said again"));
}

/// A build script reused whose `OUT_DIR` is in place writes nothing, so the walk after the
/// build that ran it is kept and the lane answers; one that wrote it again on every walk
/// would leave a racy stamp of it with each kept answer, and the lane would never answer.
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

/// What `blaze why` names: the first input that moved, by its name and never its value, and
/// nothing where the last build's own work says why the next one walks.
#[test]
fn a_refusal_names_the_input() {
    let daemon = Daemon::new();
    let (mut was, mut now) = (plain(), plain());
    now.cc = 1;
    let moved = daemon.inputs(&was).moved(&daemon.inputs(&now), &unasked);
    assert_eq!(moved, Some(Moved::Env("CC".to_owned())));
    // A variable no step read is no input, whatever it holds.
    now.shlvl = 1;
    now.cc = 0;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now), &unasked), None);
    now.cc = 1;
    assert_eq!(
        NotReplayed::Moved(Moved::Env("CC".to_owned())).input().as_deref(),
        Some("the request's variable CC")
    );
    was.source = 1;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now), &unasked), Some(Moved::Files));
    now.source = 1;
    now.cc = 0;
    now.color = true;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now), &unasked), Some(Moved::Drawing));
    now.color = false;
    now.release = true;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now), &unasked), Some(Moved::Args));
    now.release = false;
    now.test = true;
    assert_eq!(daemon.inputs(&was).moved(&daemon.inputs(&now), &unasked), Some(Moved::Command));
    assert_eq!(NotReplayed::NoAnswer(Unkept::Made).input(), None);
    assert_eq!(NotReplayed::NoAnswer(Unkept::None).input(), None);
    let looked = NotReplayed::NoAnswer(Unkept::Looked(PathBuf::from("/net/a.h"))).input();
    assert!(looked.is_some_and(|it| it.starts_with("/net/a.h, which the last build looked at")));
    let moved = NotReplayed::Output(PathBuf::from("/b/debug/app"), Unheld::Moved).input();
    assert_eq!(moved.as_deref(), Some("the output /b/debug/app"));
    let racy = NotReplayed::Leaf(PathBuf::from(HEADER), Unheld::Racy).to_string();
    assert!(racy.contains("racy rule"), "{racy}");
}

/// The answer of a walk of `inputs` that made nothing, looked as `looks` says, and read
/// nothing under its outputs, with no output: kept, or why not.
fn kept(inputs: Inputs, looks: Vec<Look>) -> Last {
    let mut recording = Recording::new(inputs, BASE_NS, Vec::new(), []);
    let none: &[PathBuf] = &[];
    recording.vouch_reads(Ok((none, none)), &Disk::new());
    recording.end(Walk { passed: true, made_nothing: true, looks, worked: None })
}

/// A request of `inputs` with nothing else in the way.
fn asked_of(inputs: &Inputs) -> Asked<'_> {
    Asked {
        inputs,
        edited: Edited::Unread("the roots are one"),
        traced: false,
        alone: true,
        locals_vouched: true,
        unasked: &unasked,
    }
}

/// A request that asks for its trace walks: there is a build to trace.
#[test]
fn a_traced_request_is_never_answered_by_the_lane() {
    let (daemon, world) = (Daemon::new(), plain());
    let inputs = daemon.inputs(&world);
    let last = kept(inputs.clone(), Vec::new());
    assert!(ask(&last, &asked_of(&inputs), &daemon.disk).is_ok());
    let traced = Asked { traced: true, ..asked_of(&inputs) };
    assert_eq!(ask(&last, &traced, &daemon.disk).err(), Some(NotReplayed::Traced));
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

/// `ops` after the builds that leave an answer kept for a build and for a test build of
/// [`plain`]: the two of [`kept_then`], the test build that makes its output, the one that
/// is kept, and a build, which is answered (the one request of these the lane answers: the
/// test build's output was placed beside the build's, another file, which moves none of the
/// build's stamps). The request is a build.
fn both_kept_then(ops: &[Op]) -> Vec<Op> {
    let kind = Op::Edit(Input::Kind);
    kept_then(&[kind, BUILD, BUILD, kind, BUILD]).into_iter().chain(ops.iter().copied()).collect()
}

/// An answer is kept for each request shape, and a request of one shape drops no other's:
/// a build, a test build, a build, a test build with nothing changed are each answered as
/// the last of its own shape was. The control is the lane until 2026-11-12, one answer a
/// checkout that any walk dropped: there every switch of request walks.
#[test]
fn an_answer_is_kept_for_each_request_shape() {
    let kind = Op::Edit(Input::Kind);
    let ops = both_kept_then(&[kind, BUILD, kind, BUILD, kind, BUILD, kind, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(5), "the build after the test build, and each after");
    // One answer a checkout: the test build's walk dropped the build's, the build's walk
    // the test build's, and so on.
    assert_eq!(runs(plain(), &ops, Some(Bug::Lane(Plant::DropsOthers))), Ok(0));
    // The third of `build`, `test`, `build` is answered where the test build placed
    // nothing in between (its output is in place): the build's answer was not dropped.
    let ops = both_kept_then(&[kind, BUILD, kind, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(3));
}

/// `build`, `test`, `build`: the third is answered though the test build made and placed its
/// output between, in the directory the build's is in: the build's answer rests on its own
/// output's stamp, not the directory's. An answer whose walk could not say what its steps
/// read under the outputs is not kept.
#[test]
fn a_build_is_answered_after_a_test_build_placed_its_output_beside() {
    let kind = Op::Edit(Input::Kind);
    let ops = kept_then(&[kind, BUILD, kind, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(1));
    let unnamed = World { names: false, ..plain() };
    assert_eq!(runs(unnamed, &ops, None), Ok(0));
}

/// A kept answer is not given after a change it rests on, whichever shape was asked
/// meanwhile: a variable the build script asks for by name changes, the build is walked
/// (the script runs again) and walked again and kept, and the test build, asked only now,
/// is not answered from the answer kept of the old value: nothing after the change is answered.
/// The control is a lane that compares no variable asked for by name, which the oracle
/// catches saying an answer made from the old value.
#[test]
fn an_answer_of_another_shape_is_not_given_after_a_change_it_rests_on() {
    let kind = Op::Edit(Input::Kind);
    let ops = both_kept_then(&[Op::Edit(Input::Made), BUILD, BUILD, kind, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(1), "the one of `both_kept_then`, none after");
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresNamed)).contains("said again"));
    // With nothing changed the same requests are answered: the build twice, the test build.
    let ops = both_kept_then(&[BUILD, BUILD, kind, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(4));
}

/// A variable nothing read changes between two requests (a shell's `SHLVL`, nushell's
/// `CMD_DURATION_MS`): the kept answer is given. The control compares the whole
/// environment, as the lane did until 2026-11-12, and walks; so does an answer whose walk
/// could not name what its steps read.
#[test]
fn a_variable_nothing_read_changes_and_the_answer_is_given() {
    let ops = kept_then(&[Op::Edit(Input::Shlvl), BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(1));
    assert_eq!(runs(plain(), &ops, Some(Bug::Lane(Plant::WholeEnvironment))), Ok(0));
    let unnamed = World { names: false, ..plain() };
    assert_eq!(runs(unnamed, &ops, None), Ok(0));
    // Each command of such a shell: every one is answered.
    let shlvl = Op::Edit(Input::Shlvl);
    let ops = kept_then(&[shlvl, BUILD, shlvl, BUILD, shlvl, BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(3));
}

/// A variable a build script asked for by name changes: the kept answer is not given, and
/// the walk runs the script again. A lane that compared only what steps read unasked would
/// say the answer made from the old value. A variable a step reads unasked (the C
/// compiler's) is compared whatever the steps named.
#[test]
fn the_oracle_catches_a_lane_that_ignores_a_variable_a_build_script_read() {
    let ops = kept_then(&[Op::Edit(Input::Made), BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(0));
    assert!(caught(plain(), &ops, Bug::Lane(Plant::IgnoresNamed)).contains("said again"));
    let ops = kept_then(&[Op::Edit(Input::Cc), BUILD]);
    assert_eq!(runs(plain(), &ops, None), Ok(0));
    assert_eq!(runs(plain(), &ops, Some(Bug::Lane(Plant::IgnoresNamed))), Ok(0));
}

/// The daemon writes a file into the checkout during a build (the lock file the checkout
/// lacks), and the watcher reports it later: the build's own view already holds it, so the
/// view the next request is asked on is the one that build was of. The cold build and the
/// one after it plan once between them; and a build after the lock file was removed walks
/// the plan it had, is kept, and the two after it are answered. The control takes the echo
/// for a change (the view recorded is the one asked, without the file): every such build is
/// followed by one that plans anew on a view no build has seen, and nothing is answered.
#[test]
fn a_file_the_build_writes_into_the_checkout_is_in_its_own_view() {
    let cold = World { lock_there: false, ..plain() };
    assert_eq!(counts(cold.clone(), &[BUILD, BUILD], None), Ok((0, 1)));
    assert_eq!(counts(cold, &[BUILD, BUILD], Some(Bug::EchoIsAChange)), Ok((0, 2)));
    let ops = kept_then(&[Op::RemoveLock, BUILD, BUILD, BUILD]);
    assert_eq!(counts(plain(), &ops, None), Ok((2, 1)));
    assert_eq!(counts(plain(), &ops, Some(Bug::EchoIsAChange)), Ok((0, 3)));
}

/// What an answer costs to keep is counted (for a footprint budget), and the store is
/// bounded by shapes: past [`MOST_SHAPES`] the least recently used goes, with what was held
/// beside it handed back.
#[test]
fn the_answers_kept_are_bounded_and_weighed() {
    let (daemon, world) = (Daemon::new(), plain());
    let inputs = daemon.inputs(&world);
    let output = Stamped {
        path: PathBuf::from(world.output_path()),
        kind: LookKind::Entry,
        stamp: Stamp::GONE,
        looked_ns: BASE_NS,
    };
    let kept = || {
        let mut recording = Recording::new(inputs.clone(), BASE_NS, vec![PathBuf::from(OUT[0])], [output.clone()]);
        let none: &[PathBuf] = &[];
        recording.vouch_reads(Ok((none, none)), &daemon.disk);
        recording.end(Walk { passed: true, made_nothing: true, looks: Vec::new(), worked: None })
    };
    let mut answers: Answers<u32> = Answers::new();
    let mut gone = Vec::new();
    for shape in 0..MOST_SHAPES as u32 + 2 {
        gone.extend(answers.keep(id(&format!("shape {shape}")), kept(), Some(shape)));
    }
    assert_eq!(gone, [0, 1], "the two least recently used");
    assert_eq!(answers.kept().count(), MOST_SHAPES);
    let one = kept().expect("kept").held_bytes();
    assert!(one >= size_of::<LastAnswer>() + OUT[0].len());
    assert_eq!(answers.held_bytes(), one * MOST_SHAPES);
    // A walk of one shape drops that shape's answer alone.
    assert_eq!(answers.begin(&id("shape 5")), [5]);
    assert_eq!(answers.kept().count(), MOST_SHAPES - 1);
    assert!(answers.answer(&id("shape 5")).is_none() && answers.answer(&id("shape 6")).is_some());
}

/// The guard of a class (rule guard-the-cause): a file the daemon writes into a checkout it
/// watches comes back from the watcher as a change, unless the build that wrote it holds it
/// in its own view first. So the jobs ask the driver to write a file in one place only,
/// `Jobs::write_locks`, which `Jobs::lock_into_view` and `Jobs::open_locked` call after the
/// file is in the job's view (a lock file outside the view is in no watched checkout); the
/// daemon's one other write is its own stall trace under its state directory. A new
/// `Effect::Write` in either file fails here until it is routed through the view or shown
/// to be outside every checkout. Outputs are placed by the store, under build directories
/// the checkout's view leaves out (`blaze_vfs::watch::fixed_out`).
#[test]
fn the_daemon_writes_into_a_checkout_only_through_the_builds_view() {
    const WRITE: &str = "Effect::Write {";
    let jobs = include_str!("../../job.rs");
    assert_eq!(jobs.matches(WRITE).count(), 0, "job.rs asks for no write but through `ViewWrite`");
    // The one write is `ViewWrite::effect`, of a value only `ViewWrite::new` makes: for a
    // path in no watched view, or one the build's view holds with the bytes written.
    let written = include_str!("../written.rs");
    assert_eq!(written.matches(WRITE).count(), 1, "one `Effect::Write`, in `ViewWrite::effect`");
    assert_eq!(written.matches("ViewWrite {").count(), 4, "the type, its impl and its two makings, both in `new`");
    // daemon.rs writes one file, the stall trace under its state directory; the only other
    // `Effect::Write` there is the planted bug of the solo rows' take's control
    // (`LEAVES_TAKEN_SOLO_ROWS`, a test build's alone), which writes a taken record back under
    // the state directory too.
    let daemon = include_str!("../../daemon.rs");
    let near = |at: usize, what: &str| daemon[..at].rfind(what).is_some_and(|it| at - it < 400);
    let (mut stalls, mut planted) = (0, 0);
    for (at, _) in daemon.match_indices(WRITE) {
        if near(at, "stalls/") {
            stalls += 1;
        } else if near(at, "LEAVES_TAKEN_SOLO_ROWS.get()") && near(at, "#[cfg(test)]") {
            planted += 1;
        } else {
            panic!("daemon.rs writes a file at byte {at} that is neither its stall trace nor the planted control's");
        }
    }
    assert_eq!((stalls, planted), (1, 1), "daemon.rs writes one file, the stall trace under the daemon's state directory");
}

/// The invariant: an answer whose walk looked at a file outside every view is said again
/// exactly while the file's stamp holds; a walk with a look that kept no stamp, or that said
/// two things, keeps no answer.
#[test]
fn a_walk_that_looked_at_a_file_is_answered_while_its_stamp_holds() {
    let (daemon, world) = (Daemon::new(), plain());
    let inputs = daemon.inputs(&world);
    let mut disk = Disk::new();
    disk.write("/root");
    disk.tick();
    let stamp = disk.at("/root");
    let last = kept(inputs.clone(), vec![look("/root", LookKind::File, Some([7; 32]), stamp, disk.clock)]);
    disk.tick();
    assert!(ask(&last, &asked_of(&inputs), &disk).is_ok());
    disk.write("/root");
    let moved = NotReplayed::Leaf(PathBuf::from("/root"), Unheld::Moved);
    assert_eq!(ask(&last, &asked_of(&inputs), &disk).err(), Some(moved));
    let unstamped = kept(inputs.clone(), vec![look("/net/dir", LookKind::Entry, None, Stamp::UNKNOWN, disk.clock)]);
    assert!(matches!(&unstamped, Err(Unkept::Looked(path)) if path == Path::new("/net/dir")));
    // A file looked at twice, saying two things, keeps none either: one stamp can't stand
    // for both.
    let twice = vec![
        look("/root", LookKind::File, None, stamp, disk.clock),
        look("/root", LookKind::File, Some([1; 32]), stamp, disk.clock),
    ];
    assert!(matches!(&kept(inputs, twice), Err(Unkept::Looked(path)) if path == Path::new("/root")));
}

/// The control: a look that kept no stamp keeps the answer, which nothing could sweep.
#[test]
fn the_oracle_catches_a_kept_look_with_no_stamp() {
    let inputs = Daemon::new().inputs(&plain());
    PLANTED.set(Some(Plant::KeepsALook));
    let last = kept(inputs, vec![look("/net/dir", LookKind::Entry, None, Stamp::UNKNOWN, BASE_NS)]);
    PLANTED.set(None);
    assert!(last.is_ok(), "planted: the look with no stamp is not refused");
}

/// The sources that read the disk outside a checkout, or keep or check what was read there:
/// none may ask the driver for a watch ([`asks_a_watch`]).
const READERS: [(&str, &str); 11] = [
    ("blaze-vfs/src/outside.rs", include_str!("../../../../../blaze-vfs/src/outside.rs")),
    ("blaze-vfs/src/lib.rs", include_str!("../../../../../blaze-vfs/src/lib.rs")),
    ("blaze-usercode/src/inputs.rs", include_str!("../../../../../blaze-usercode/src/inputs.rs")),
    ("base-db/src/lib.rs", include_str!("../../../../../base-db/src/lib.rs")),
    ("blaze-host/src/outside.rs", include_str!("../../../../../blaze-host/src/outside.rs")),
    ("blaze-cargo/src/serve/job.rs", include_str!("../../job.rs")),
    ("blaze-cargo/src/serve/job/certify.rs", include_str!("../certify.rs")),
    ("blaze-cargo/src/serve/heads.rs", include_str!("../../heads.rs")),
    ("blaze-cargo/src/serve/daemon.rs", include_str!("../../daemon.rs")),
    ("blaze-cargo/src/serve/mounts.rs", include_str!("../../mounts.rs")),
    ("blaze-cargo/src/serve/fetch/local.rs", include_str!("../../fetch/local.rs")),
];

/// What making a watch reads as in a source: the effect with the directory it is of. Split so
/// this file is not one.
const WATCH: &str = concat!("Effect::", "Watch { dir");

/// The oracle of "no read outside a checkout asks for a watch": the first of `sources` that
/// makes an `Effect::Watch`, named.
fn asks_a_watch(sources: &[(&str, &str)]) -> Result<(), String> {
    match sources.iter().find(|(_, text)| text.contains(WATCH)) {
        Some((name, _)) => Err(format!("an outside read asks for a watch: `{WATCH}` in {name}")),
        None => Ok(()),
    }
}

/// The invariant: a build script's read of one name in a directory of a million entries
/// costs one stamp of that name when its answer is asked again, and nothing outside a
/// checkout is watched: no source that reads, keeps or checks the disk there makes an
/// `Effect::Watch`. Until 2026-11-14 such a read asked a watch of the directory, and setting
/// it up walked the tree under it on the daemon's loop (256,216 directories under `/root`).
#[test]
fn an_outside_read_costs_one_stamp_and_asks_no_watch() {
    const BIG: &str = "/root/big";
    const NAME: &str = "/root/big/name";
    let inputs = Daemon::new().inputs(&plain());
    let mut disk = Disk::new();
    // A directory of a million entries: the model holds none of them, and a stamp asked of
    // any path but the name is counted all the same.
    disk.put(BIG, DIR, true);
    disk.write(NAME);
    disk.tick();
    let looks = blaze_vfs::outside::Looks::new();
    looks.push_stamped(Path::new(NAME), LookKind::File, Some([3; 32]), disk.at(NAME), disk.clock);
    let last = kept(inputs, looks.all()).expect("a walk whose one look kept a stamp is kept");
    assert_eq!(last.leaves.len(), 1);
    assert!(last.outputs.is_empty());
    disk.tick();
    disk.asked.set(0);
    assert_eq!(last.sweep(&disk), Ok(()));
    assert_eq!(disk.asked.get(), 1, "one stamp, of the name read: none of its directory or entries");
    asks_a_watch(&READERS).unwrap();
}

/// The control: an outside read that asks for a watch, planted beside the readers.
#[test]
fn the_oracle_catches_an_outside_read_that_asks_for_a_watch() {
    let mut planted = READERS.to_vec();
    let watch = format!("effects.push({WATCH}: file, tag }});");
    planted.push(("blaze-vfs/src/outside.rs, planted", &watch));
    let why = asks_a_watch(&planted).expect_err("the oracle missed the planted bug");
    assert!(why.starts_with("an outside read asks for a watch") && why.contains("planted"), "{why}");
}
