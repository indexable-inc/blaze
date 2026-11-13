//! What the thin `blaze` says to the daemon for one command, and what it comes to: one
//! connection, on which it asks which engine the daemon is ([`STATUS`]), sends the command's
//! request ([`BUILD`]) and, for a `run` whose answer it prints, asks for the program's launch
//! ([`LAUNCH`]); as the command ends, the program (`main.rs`) sends the command's own spans
//! ([`Timings`], [`ROWS`]) on it last, waiting for no answer.
//!
//! The rule this holds, and its test counts (`tests::a_run_after_an_edit_is_one_connection_and_one_status`):
//! a command is one connection and at most one `Status`. [`talk`] is given the one connection
//! as a [`Link`] and has no way to make another; what it sends is what the link counts. Until
//! 2026-11-11 an edit's `run` was three connections (the build's, the launch's and the
//! client's rows', the last two the engine's after the handover).
//!
//! A sans-IO core: the link's bytes and waits are the program's (`main.rs`), and a test's
//! link is a script of responses.

use std::{path::Path, time::Duration};

use blaze_api::{Envelope, Executable, Request, Response};

use crate::{
    answer::{Exec, Plain, Shown, Step},
    handover,
    route::{Asked, Command},
};

/// The request ids of a command's connection, in the order they are sent.
pub const STATUS: u64 = 1;
pub const BUILD: u64 = 2;
pub const LAUNCH: u64 = 3;
/// The command's own spans ([`Timings`]), sent last as a `ClientRows`, answered by nobody: the
/// command has ended (a `build`) or become its program (a `run`) by then.
pub const ROWS: u64 = 4;

/// The names of the thin `blaze`'s spans, each opened and closed in one place (handoff-12
/// request 2): from the kernel's start of this process to `main`'s first line (`main.rs`),
/// the client's own check of the request slot (`main.rs`, around `solo::solo`), the connect
/// (`main.rs`), the build's request from its send to its answer's `Done` ([`talk`]), and from
/// that `Done` to the program's exec ([`blaze_api::Timed::ANSWER_TO_EXEC`], `main.rs`).
pub const EXEC_TO_MAIN: &str = "client: exec to main";
pub const SOLO_CHECK: &str = "client: solo check";
pub const CONNECT: &str = "client: connect";
pub const ON_THE_WIRE: &str = "client: request on the wire";

/// What the command timed of itself, on the profile's clock (`blaze_profile::clock::mono_ns`,
/// [`Link::now_ns`]), and the build the daemon said its request is: sent to the daemon as
/// the command ends, on its one connection ([`ROWS`]), as that build's client rows.
#[derive(Debug, Default)]
pub struct Timings {
    pub timed: Vec<blaze_api::Timed>,
    /// [`blaze_api::Worked::build_id`] of the answer, once heard.
    pub build: Option<u64>,
    /// When the build's answer ended (`Done`): where `client: answer to exec` starts.
    pub answered_ns: Option<i64>,
}

impl Timings {
    /// The span `name` from `start_ns` to `end_ns`.
    pub fn span(&mut self, name: &str, start_ns: i64, end_ns: i64) {
        // alloc: a span's name, a few a command
        self.timed.push(blaze_api::Timed { name: name.to_owned(), start_ns, end_ns });
    }

    /// The command's rows for its build, `pid` its process, from `started_ns` (the kernel's
    /// start of it) to `ended_ns`: `None` when no build was heard, which has no rows.
    pub fn rows(self, pid: u32, started_ns: i64, ended_ns: i64) -> Option<Request> {
        let build = self.build?;
        Some(Request::ClientRows {
            pid,
            build,
            spans: Vec::new(),
            timed: self.timed,
            events: Vec::new(),
            folded: Vec::new(),
            ended_ns,
            started_ns,
        })
    }
}

/// How long the daemon is given to say which engine it is, and to answer a launch: both
/// are answered by its loop as they are read. Past it the engine runs the command, and
/// says what the daemon is doing.
pub const ASKED_WITHIN: Duration = Duration::from_secs(2);

/// What a read of the daemon's connection came to.
#[allow(clippy::large_enum_variant, reason = "one a read, matched and moved at once")]
#[derive(Debug)]
pub enum Read {
    Response(Envelope<Response>),
    Deadline,
    HungUp,
}

/// The one connection of a command, as [`talk`] uses it: requests out, responses in, each
/// wait until a deadline the link makes on its own clock.
pub trait Link {
    /// A deadline on the link's clock.
    type Until: Copy;

    /// `wait` from now.
    fn within(&self, wait: Duration) -> Self::Until;

    /// Sends `request` under `id`: its frame as sent.
    fn send(&mut self, id: u64, request: &Request) -> Result<Vec<u8>, String>;

    /// The next response, waiting for the daemon until `until`; `what` names the wait.
    fn next(&mut self, until: Self::Until, what: &'static str) -> Result<Read, String>;

    /// How many bytes are read that are not yet a whole response.
    fn held(&self) -> usize;

    /// Lets go of the bytes read so far: what is read from here on is the next request's
    /// answer, which a handover carries whole.
    fn forget(&mut self);

    /// Now on the profile's clock (`blaze_profile::clock::mono_ns`): what [`Timings`] are
    /// read on.
    fn now_ns(&self) -> i64;
}

/// The command [`talk`] speaks for.
pub struct Spoken<'a> {
    pub asked: &'a Asked,
    /// The command's request, the CLI's own ([`crate::request::request`]).
    pub request: &'a Request,
    /// How the command shows its answer: what stderr is, as the program found it.
    pub shown: &'a Shown,
    /// The directory the command runs in, which a `run`'s program is shown and started from.
    pub cwd: &'a Path,
    /// This process, which a `run` becomes its program as.
    pub pid: u32,
}

/// What a command's talk with the daemon came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// Before the request was sent, or with nothing written and nothing the engine would
    /// not ask again: the engine runs the command line as typed, for this reason.
    Engine(String),
    /// The request is sent and the answer is the engine's to print: it takes the connection
    /// over, with the request's frame as sent (`request`) and what the link read since its
    /// last [`Link::forget`].
    HandOver { request: Vec<u8> },
    /// The answer is printed here: `said` goes to stderr, and a `run` then becomes `exec`.
    Said { said: String, exec: Option<Exec> },
}

/// One command's talk on `link`: the daemon asked which engine it is (`is_engine` says whether
/// its executable is the one beside this program), the request sent, its answer taken, and a
/// `run`'s launch asked. `took` is the time since the command began, read as the answer ends.
/// `timings` gets the request's time on the wire ([`ON_THE_WIRE`]) and its build's id.
pub fn talk<L: Link>(
    link: &mut L,
    spoken: &Spoken<'_>,
    is_engine: impl FnOnce(&Executable) -> Result<(), String>,
    took: impl FnOnce() -> Duration,
    timings: &mut Timings,
) -> Ended {
    match before(link, is_engine) {
        Ok(()) => {}
        Err(why) => return Ended::Engine(why),
    }
    // From here the request is sent: what follows ends in this program's own answer or
    // in the engine taking the answer over, never in the command asked a second time.
    let sent_ns = link.now_ns();
    let request = match link.send(BUILD, spoken.request) {
        Ok(frame) => frame,
        Err(why) => return Ended::Engine(why),
    };
    let asked = spoken.asked;
    let until = link.within(handover::AFTER);
    let mut plain = Plain::of(asked.command == Command::Build && !asked.quiet);
    // Bounded: each turn takes one response of an answer that ends, or ends at the deadline.
    loop {
        match link.next(until, "the daemon's answer to the request") {
            Ok(Read::Response(Envelope { id: BUILD, body, .. })) => {
                if let Response::Work(worked) = &body {
                    timings.build = worked.build_id;
                }
                match plain.take(body) {
                    Step::More => {}
                    Step::Done => {
                        let answered = link.now_ns();
                        timings.span(ON_THE_WIRE, sent_ns, answered);
                        timings.answered_ns = Some(answered);
                        break;
                    }
                    Step::HandOver(_) => return Ended::HandOver { request },
                }
            }
            // A response to no request of this connection, an answer not ended in time,
            // a daemon that hung up or bytes that are not the API's: the engine reads what
            // was read and says which.
            Ok(Read::Response(_) | Read::Deadline | Read::HungUp) | Err(_) => {
                return Ended::HandOver { request };
            }
        }
    }
    if link.held() != 0 {
        return Ended::HandOver { request };
    }
    // `run`: the one program of what was built, started as the daemon says to start it.
    // clone: the launch's program, once a run
    let executable = plain.program().and_then(|it| it.executable.clone());
    let Ok((said, exec)) = said(spoken, plain, took()) else { return Ended::HandOver { request } };
    let (Some(mut exec), Some(executable)) = (exec, executable) else { return Ended::Said { said, exec: None } };
    // The answer is whole and plain, and nothing is written yet: a launch the daemon
    // refuses, or does not answer, is the engine's to say, which runs the command again
    // (its build is answered as this one was).
    match launch(link, executable, spoken.pid) {
        Ok(Some(env)) => exec.env.extend(env),
        Ok(None) => {}
        Err(why) => return Ended::Engine(why),
    }
    Ended::Said { said, exec: Some(exec) }
}

/// What a whole, plain answer comes to for the command `spoken`, its build over `took`: what
/// is written to stderr (the screen's, docs/terminal.md: nothing of the program's command
/// line, which is said only under `-v`, the engine's) and, for a `run`, the one program of
/// what was built, which the command becomes. `Err`: a `run` whose build has no one program
/// to run (none, several, or one with a runner), which the engine says: [`talk`] hands the
/// answer over, `crate::solo` asks the daemon.
#[allow(clippy::result_unit_err, reason = "one reason, which each caller names its own way")]
pub fn said(spoken: &Spoken<'_>, plain: Plain, took: Duration) -> Result<(String, Option<Exec>), ()> {
    let asked = spoken.asked;
    let said = plain.said(spoken.shown, asked.quiet, took);
    if asked.command == Command::Build {
        return Ok((said, None));
    }
    let program = plain.program().and_then(|it| Some((it.executable.as_deref()?, it.env.as_slice())));
    let Some((executable, cargo_env)) = program else { return Err(()) };
    Ok((said, Some(Exec::of(executable, cargo_env, &asked.program, spoken.cwd))))
}

/// No request of this blaze runs on another's daemon: the daemon says which engine it is,
/// and any but the one beside this program is the engine's to replace.
fn before<L: Link>(
    link: &mut L,
    is_engine: impl FnOnce(&Executable) -> Result<(), String>,
) -> Result<(), String> {
    link.send(STATUS, &Request::Status)?;
    let mut status = None;
    let until = link.within(ASKED_WITHIN);
    // Bounded: a status and its `Done`, or the deadline.
    loop {
        match link.next(until, "the daemon to say which blaze it is")? {
            Read::Response(Envelope { id: STATUS, body: Response::Status(it), .. }) => status = Some(it),
            Read::Response(Envelope { id: STATUS, body: Response::Done { ok: true }, .. }) => break,
            Read::Response(_) => return Err("the daemon answered its status with something else".to_owned()),
            Read::Deadline => return Err("the daemon did not say which blaze it is in time".to_owned()),
            Read::HungUp => return Err("the daemon hung up".to_owned()),
        }
    }
    let status = status.ok_or("the daemon answered its status with no status")?;
    is_engine(&status.build.exe)?;
    if link.held() != 0 {
        return Err("the daemon sent more after its status".to_owned());
    }
    link.forget();
    Ok(())
}

/// The daemon's answer to starting `executable` as process `pid`, asked on the command's own
/// connection: the start hook's variables when it runs on the daemon's code memory, `None`
/// when it runs its file.
fn launch<L: Link>(
    link: &mut L,
    executable: String,
    pid: u32,
) -> Result<Option<Vec<(String, String)>>, String> {
    let launch = Request::Launch { program: executable, pid };
    link.send(LAUNCH, &launch)?;
    let until = link.within(ASKED_WITHIN);
    let mut launched = None;
    // Bounded: a `Launched` and its `Done`, or the deadline.
    loop {
        match link.next(until, "the daemon's answer to the program's launch")? {
            Read::Response(Envelope { id: LAUNCH, body: Response::Launched { env, live }, .. }) => {
                launched = Some((env, live));
            }
            Read::Response(Envelope { id: LAUNCH, body: Response::Done { ok: true }, .. }) => break,
            Read::Response(_) => return Err("the daemon refused the program's launch".to_owned()),
            Read::Deadline => return Err("the daemon did not answer the program's launch in time".to_owned()),
            Read::HungUp => return Err("the daemon hung up".to_owned()),
        }
    }
    let (env, live) = launched.ok_or("the daemon answered the launch with no `Launched`")?;
    Ok(live.then_some(env))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, ffi::OsString};

    use blaze_api::{
        Artifact, Build, LinkHow, LinkKind, Linked, PlacedBytes, Status, UnitStarted, Verb, Worked,
    };

    use super::*;
    use crate::route::{self, Route};

    /// A daemon as a script: each request sent is counted by what it is, and answered with the
    /// next responses of the script for its id. The one connection a command has.
    struct Scripted {
        answers: VecDeque<Envelope<Response>>,
        /// Every request sent, by its id.
        sent: Vec<u64>,
        statuses: u32,
        launches: u32,
        builds: u32,
        forgotten: u32,
    }

    impl Scripted {
        fn answering(answers: Vec<(u64, Response)>) -> Scripted {
            Scripted {
                answers: answers.into_iter().map(|(id, body)| Envelope::new(id, body)).collect(),
                sent: Vec::new(),
                statuses: 0,
                launches: 0,
                builds: 0,
                forgotten: 0,
            }
        }
    }

    impl Link for Scripted {
        type Until = ();

        fn within(&self, _: Duration) {}

        fn send(&mut self, id: u64, request: &Request) -> Result<Vec<u8>, String> {
            self.sent.push(id);
            match request {
                Request::Status => self.statuses += 1,
                Request::Launch { .. } => self.launches += 1,
                _ => self.builds += 1,
            }
            blaze_api::frames::encode(&Envelope::new(id, request)).map_err(|error| error.what.to_string())
        }

        fn next(&mut self, (): (), _: &'static str) -> Result<Read, String> {
            // A response is given only once its request was sent: the script is a daemon's.
            match self.answers.front() {
                Some(next) if self.sent.contains(&next.id) => {
                    Ok(Read::Response(self.answers.pop_front().expect("there is a front")))
                }
                Some(_) | None => Ok(Read::Deadline),
            }
        }

        fn held(&self) -> usize {
            0
        }

        fn forget(&mut self) {
            self.forgotten += 1;
        }

        /// A clock that moves only as requests are sent: 10 a request.
        fn now_ns(&self) -> i64 {
            self.sent.len() as i64 * 10
        }
    }

    /// The command's request, as far as the script's daemon reads it: any that is neither a
    /// `Status` nor a launch.
    const COMMAND: Request = Request::Stop;

    fn asked(typed: &[&str]) -> Asked {
        let words: Vec<OsString> = typed.iter().map(OsString::from).collect();
        match route::words(&words) {
            Route::Cargo(asked) => asked,
            other => panic!("{typed:?} is not the thin blaze's: {other:?}"),
        }
    }

    fn status() -> Vec<(u64, Response)> {
        let exe = Executable { path: "/b/blaze-engine".to_owned(), len: 1, inode: 2, modified_ns: 3 };
        let status = Status {
            pid: 7,
            bugs: Vec::new(),
            build: Build { id: blaze_ids::ContentId::of("test", b"engine"), exe },
            views: 0,
            builds: Vec::new(),
            history: None,
            requests: Vec::new(),
            settings: Vec::new(),
            downloads: Vec::new(),
            uptime_ms: 0,
            placed_by: None,
            budget_bytes: 0,
            memory_bytes: 0,
        };
        vec![(STATUS, Response::Status(status)), (STATUS, Response::Done { ok: true })]
    }

    fn artifact() -> Artifact {
        Artifact {
            env: Vec::new(),
            kind: blaze_api::ArtifactKind::Bin,
            path: "/w/target/debug/foo".to_owned(),
            fresh: false,
            runner: Vec::new(),
            target: blaze_api::UnitTarget {
                doc: false,
                kind: blaze_api::TargetKind::Bin,
                name: "foo".to_owned(),
                test: false,
                doctest: false,
                edition: "2024".to_owned(),
                src_path: "/w/foo/src/main.rs".to_owned(),
                crate_types: Vec::new(),
            },
            content: blaze_ids::ContentId::of("test", b"foo"),
            harness: false,
            package: "path+file:///w/foo#0.1.0".to_owned(),
            profile: blaze_api::UnitProfile {
                test: false,
                debuginfo: 2,
                opt_level: "0".to_owned(),
                overflow_checks: true,
                debug_assertions: true,
            },
            features: Vec::new(),
            filenames: vec!["/w/target/debug/foo".to_owned()],
            executable: Some("/w/target/debug/foo".to_owned()),
            manifest_path: "/w/foo/Cargo.toml".to_owned(),
        }
    }

    /// An edit's answer: a unit compiled, the outputs, the work with a step computed again and
    /// its image spliced, the `Done`.
    fn edit() -> Vec<(u64, Response)> {
        let unit = UnitStarted {
            name: "foo".to_owned(),
            verb: Verb::Compiling,
            source: Some("(/w/foo)".to_owned()),
            package: 0,
            version: "0.1.0".to_owned(),
            package_id: "path+file:///w/foo#0.1.0".to_owned(),
        };
        let built = Response::Built {
            members: 1,
            profile: "`dev` profile [unoptimized + debuginfo]".to_owned(),
            artifacts: vec![artifact()],
        };
        let worked = Worked {
            why: None,
            storage: None,
            link: LinkKind::Patched,
            found: 0,
            green: 4,
            links: vec![Linked { how: LinkHow::Spliced, path: "/w/target/debug/foo".to_owned() }],
            reran: 3,
            misses: 3,
            placed: PlacedBytes::default(),
            served: Vec::new(),
            build_id: Some(7),
            compiled: 1,
            steps_us: Vec::new(),
            critical_path_us: 0,
            tiers: None,
        };
        vec![
            (BUILD, Response::Unit(unit)),
            (BUILD, built),
            (BUILD, Response::Work(worked)),
            (BUILD, Response::Done { ok: true }),
        ]
    }

    fn launched() -> Vec<(u64, Response)> {
        vec![
            (LAUNCH, Response::Launched { env: Vec::new(), live: false }),
            (LAUNCH, Response::Done { ok: true }),
        ]
    }

    fn talked(typed: &[&str], answers: Vec<(u64, Response)>) -> (Ended, Scripted) {
        let asked = asked(typed);
        let mut link = Scripted::answering(answers);
        let shown = Shown::pipe(blaze_screen::Command::Run, false);
        let spoken = Spoken {
            asked: &asked,
            request: &COMMAND,
            shown: &shown,
            cwd: Path::new("/w"),
            pid: 41,
        };
        let ended = talk(&mut link, &spoken, |_| Ok(()), || Duration::ZERO, &mut Timings::default());
        (ended, link)
    }

    /// The invariant: a `run` after an edit, answered here, times its request on the wire
    /// exactly once, from its send to its answer's `Done`, hears its build's id, and its rows
    /// are that build's with each of the client's spans once and nothing of another's.
    /// Controls: an answer handed to the engine times no wire (its spans are the engine's),
    /// and a command whose answer named no build has no rows.
    #[test]
    fn a_run_times_its_request_on_the_wire_once_and_its_rows_are_its_builds() {
        let asked = asked(&["run", "-q"]);
        let mut link = Scripted::answering([status(), edit(), launched()].concat());
        let shown = Shown::pipe(blaze_screen::Command::Run, false);
        let spoken = Spoken { asked: &asked, request: &COMMAND, shown: &shown, cwd: Path::new("/w"), pid: 41 };
        let mut timings = Timings::default();
        timings.span(EXEC_TO_MAIN, 0, 1);
        timings.span(CONNECT, 1, 2);
        let ended = talk(&mut link, &spoken, |_| Ok(()), || Duration::ZERO, &mut timings);
        assert!(matches!(ended, Ended::Said { exec: Some(_), .. }), "{ended:?}");
        // Sent after the `Status`: the clock read 10 then, and 20 once the build was sent.
        assert_eq!(timings.answered_ns, Some(20));
        assert_eq!(timings.build, Some(7));
        timings.span(blaze_api::Timed::ANSWER_TO_EXEC, 20, 25);
        let Some(Request::ClientRows { pid, build, timed, spans, started_ns, ended_ns, .. }) = timings.rows(41, 0, 25)
        else {
            panic!("a build was heard, so there are rows");
        };
        assert_eq!((pid, build, started_ns, ended_ns, spans.len()), (41, 7, 0, 25, 0));
        for name in [EXEC_TO_MAIN, CONNECT, ON_THE_WIRE, blaze_api::Timed::ANSWER_TO_EXEC] {
            assert_eq!(timed.iter().filter(|it| it.name == name).count(), 1, "{name}: {timed:?}");
        }
        assert_eq!(timed.len(), 4, "{timed:?}");
        assert_eq!(blaze_api::Timed::exec_of(&timed), Some(25));

        // The controls.
        let asked = asked_build();
        let mut link = Scripted::answering([status(), edit()].concat());
        let spoken = Spoken { asked: &asked, request: &COMMAND, shown: &shown, cwd: Path::new("/w"), pid: 41 };
        let mut timings = Timings::default();
        let ended = talk(&mut link, &spoken, |_| Ok(()), || Duration::ZERO, &mut timings);
        assert!(matches!(ended, Ended::HandOver { .. }), "{ended:?}");
        assert!(timings.timed.iter().all(|it| it.name != ON_THE_WIRE), "{:?}", timings.timed);
        assert_eq!(Timings::default().rows(41, 0, 1), None);
    }

    fn asked_build() -> Asked {
        asked(&["build"])
    }

    /// The handshake's count, which no timing changes: a `run` after an edit (a unit compiled,
    /// its image spliced) is answered on the one connection it is given, with one `Status`, the
    /// build's request and one launch, in that order, and ends in the program's exec with
    /// nothing handed to the engine. A `build -q` of the same is one `Status` and its request.
    #[test]
    fn a_run_after_an_edit_is_one_connection_and_one_status() {
        let answers = || [status(), edit(), launched()].concat();
        for typed in [&["run", "-q"][..], &["run"]] {
            let (ended, link) = talked(typed, answers());
            assert_eq!((link.statuses, link.builds, link.launches), (1, 1, 1), "{typed:?}");
            assert_eq!(link.sent, [STATUS, BUILD, LAUNCH], "{typed:?}");
            let Ended::Said { said, exec: Some(exec) } = ended else { panic!("{typed:?}: {ended:?}") };
            assert_eq!(exec.program, Path::new("target/debug/foo"), "{typed:?}");
            // The screen's one line of the edit's build, to a pipe; nothing under `-q`.
            match typed.contains(&"-q") {
                true => assert_eq!(said, "", "{typed:?}"),
                false => assert!(said.starts_with("blaze: ok built="), "{typed:?}: {said}"),
            }
        }
        let (ended, link) = talked(&["build", "-q"], answers());
        assert_eq!((link.statuses, link.builds, link.launches), (1, 1, 0));
        assert_eq!(
            ended,
            Ended::Said { said: String::new(), exec: None }
        );
    }

    /// The controls. A `build` that says why it recomputed is handed over once its request is
    /// sent, having asked one `Status` and nothing after; a daemon that is not the engine
    /// beside this program is the engine's before anything but that `Status` is sent; and a
    /// launch not answered sends the command to the engine, with no second `Status`.
    #[test]
    fn what_is_not_printed_here_is_handed_on_with_one_status() {
        let (ended, link) = talked(&["build"], [status(), edit()].concat());
        assert!(matches!(ended, Ended::HandOver { .. }), "{ended:?}");
        assert_eq!((link.statuses, link.builds, link.launches, link.forgotten), (1, 1, 0, 1));

        let asked = asked(&["run", "-q"]);
        let mut link = Scripted::answering(status());
        let shown = Shown::pipe(blaze_screen::Command::Run, false);
        let spoken =
            Spoken { asked: &asked, request: &COMMAND, shown: &shown, cwd: Path::new("/w"), pid: 41 };
        let ended =
            talk(&mut link, &spoken, |_| Err("another build".to_owned()), || Duration::ZERO, &mut Timings::default());
        assert_eq!(ended, Ended::Engine("another build".to_owned()));
        assert_eq!(link.sent, [STATUS]);

        let (ended, link) = talked(&["run", "-q"], [status(), edit()].concat());
        assert!(matches!(ended, Ended::Engine(_)), "{ended:?}");
        assert_eq!((link.statuses, link.builds, link.launches), (1, 1, 1));
    }
}
