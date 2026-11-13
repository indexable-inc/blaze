//! The daemon's side of a launched program across its start (`serve::unit::run::launch`, the
//! launch host): the start images the host keeps are put in the home's store and marked in
//! the journal once their puts are durable, so the next start hands the marks back and a
//! program is restored from its image with no build; and a first call no record answers,
//! where no task holds the program's front end, is made by a build the daemon sends itself:
//! the build it last answered for that program. `blaze run` asks the daemon for a build
//! (`Request::Build`, its cargo line saying `run`) and starts the program itself, so that
//! build is sent again as it came; an agent's `Request::Run`, which the daemon carries out
//! by running the program too, is not, as sending it again would run the program again.
//!
//! The order is the rule (rule 43, `serve::tests::launched_restart`): a mark is written only
//! after the store says its image's put is durable, so a crash at any point leaves a start
//! with no mark, the older one, or one whose image the store holds byte for byte. Once marked,
//! the image is placed beside the program as its start file ([`crate::serve::job::slot::Placing`]):
//! written to a part file, synced, renamed over (a rename that waits for the drive), the
//! directory synced, so a crash leaves no start file, the older one, or this one whole.

use super::*;
use crate::serve::local::{LAUNCH, LaunchMark};
use crate::serve::job::slot::{Place, Placing};
use crate::serve::work::LaunchMarked;

/// The tags of start files' places: one a place, its number past [`super::PLACES`].
const PLACES_MOST: u64 = 1 << 32;

/// A start image put and not yet durable, one a program.
pub(super) struct Due {
    /// The mark's name.
    name: String,
    /// The mark's bytes.
    mark: Vec<u8>,
    /// The put's ticket.
    ticket: blaze_cas::Ticket,
    /// The image's bytes, placed as the start file beside each of `beside` once marked.
    image: Vec<u8>,
    /// The program's path and the path it runs from (once where they are one).
    beside: Vec<String>,
}

/// A start file being placed: the steps left, and the file named in its errors.
struct Placed {
    number: u64,
    file: String,
    placing: Placing,
}

/// The connection the daemon's own `blaze run`s for first calls are of: no client's, and
/// not the build ahead's ([`super::ahead::CONN`]).
pub(super) const CONN: blaze_core::Conn = blaze_core::Conn(u64::MAX - 1);

#[cfg(test)]
thread_local! {
    /// A planted bug for `serve::tests::launched_restart`'s control: a start image is marked
    /// as it is put, before the store says the put is durable, so a crash may leave a mark
    /// naming an image the store lost.
    pub(in crate::serve) static MARKS_BEFORE_DURABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What the daemon keeps for launched programs between turns.
#[derive(Default)]
pub(super) struct Launch {
    /// Each client's build (`Request::Build`) under way, by its request: what its `Built`
    /// answer ties to the programs it built.
    runs: Vec<(RequestRef, Request)>,
    /// By the path each program runs from (`Artifact::executable`, what a build's answer names
    /// it by, and what the launch host's wants name it by), the build this daemon last answered
    /// with that program built: what it sends itself for a first call no record answers.
    programs: Vec<(String, Request)>,
    /// Start images put and not yet durable, one a program: the mark's name, its bytes, the
    /// put's ticket. Marked, and taken out, once the ticket is durable.
    pub(super) due: Vec<Due>,
    /// Start files being placed, each by the number its tag carries.
    placing: Vec<Placed>,
    /// The last place's number.
    places: u64,
    /// The daemon's own runs under way: the request's id, the program it is for, the error
    /// it was answered with if any, and the span `launch: the daemon builds a program for a
    /// first call` from its send to its `Done`.
    sent: Vec<(u64, String, Option<String>, tracing::Span)>,
    /// The last id of the daemon's own runs.
    next: u64,
}

impl Launch {
    /// Whether anything of a start image is not yet durable: a put not yet marked, or a
    /// start file not yet placed.
    pub(super) fn undurable(&self) -> bool {
        !self.due.is_empty() || !self.placing.is_empty()
    }
}

/// The journal's marks of start images, as the daemon's start hands them to the launch host:
/// each program's path and the path it runs from, the record key of the build its image holds
/// and the image's key. A
/// mark that does not decode is said at error, naming it, and taken out of the journal: the
/// program is then built, as one no image holds.
pub(super) fn marked(vfs: &mut Vfs) -> Vec<LaunchMarked> {
    // alloc: once a start, a mark a launched program
    let read: Vec<(String, Result<LaunchMark, crate::serve::local::Undecoded>)> = vfs
        .marks(LAUNCH)
        .map(|(name, bytes)| (name.to_owned(), LaunchMark::decode(name, bytes)))
        .collect();
    let mut marked = Vec::with_capacity(read.len()); // alloc: once a start
    for (name, mark) in read {
        match mark {
            Ok(mark) => marked.push(LaunchMarked {
                program: name[LAUNCH.len()..].to_owned(), // alloc: once a start, a mark
                runs_as: mark.runs_as,
                record: mark.record,
                key: mark.start,
            }),
            Err(why) => {
                tracing::error!(target: "blaze", "the start image mark {name:?} is let go of, and its program is built: {why}");
                _ = vfs.unmark(&name);
            }
        }
    }
    marked
}

impl<D: Driver, S: Steps> Daemon<D, S> {
    /// Each turn: the start images the launch host kept put in the home's store, each marked
    /// once its put is durable; and for each program a first call of which waits for a front
    /// end no task holds, the `blaze run` last answered for it sent to the session as a
    /// client's, or, with none, the need ended by name.
    pub(super) fn launch_turn(&mut self) {
        // The programs that ended since the last turn: their spans to the profile.
        for ran in self.steps.launch_spans() {
            self.observe.program_rows(ran);
        }
        let kept = self.steps.launch_kept();
        let mut wants = self.steps.launch_wants();
        // One build a program, however many first calls asked.
        wants.sort_unstable();
        wants.dedup();
        if kept.is_empty() && wants.is_empty() && self.launch.due.is_empty() {
            return;
        }
        let _span = tracing::info_span!(
            target: "blaze",
            "launch: keep start images and build for first calls",
            kept = kept.len() as u64,
            wants = wants.len() as u64,
            due = self.launch.due.len() as u64,
        )
        .entered();
        let mut put = false;
        for image in kept {
            // alloc: a mark's name, once an image
            let name = format!("{LAUNCH}{}", image.program);
            // clone: the name the program runs as, in its mark and beside its image
            let mark = LaunchMark { record: image.record, start: image.key, runs_as: image.runs_as.clone() }.encode();
            match self.store.put(image.key, blaze_cas::Kind::Code, &image.bytes) {
                Ok(ticket) => {
                    put = true;
                    // A newer image of a program replaces one still waiting: only the newest is
                    // ever marked.
                    self.launch.due.retain(|it| it.name != name);
                    #[cfg(test)]
                    if MARKS_BEFORE_DURABLE.get() {
                        // The planted bug marks as the real path does once durable, flush
                        // included: a mark left unflushed in the journal reaches the disk only
                        // after the image does, and the control would plant nothing.
                        if self.mark_launch(&name, &mark) {
                            self.vfs.flush(&mut self.effects);
                        }
                        continue;
                    }
                    let mut beside = vec![image.program]; // alloc: once an image, its one or two paths
                    if image.runs_as != beside[0] {
                        beside.push(image.runs_as);
                    }
                    self.launch.due.push(Due { name, mark, ticket, image: image.bytes, beside });
                }
                Err(mismatch) => tracing::error!(
                    target: "blaze",
                    "{}: its start image is not kept: the store holds other bytes under its key {:?} ({:?}, given {:?})",
                    image.program,
                    mismatch.key,
                    mismatch.kept,
                    mismatch.given,
                ),
            }
        }
        if put {
            self.store.flush(&mut self.effects);
        }
        let mut marked = false;
        let mut place = Vec::new(); // alloc: none until an image is durable
        let mut due = std::mem::take(&mut self.launch.due);
        due.retain_mut(|it| {
            if !self.store.durable(it.ticket) {
                return true;
            }
            marked |= self.mark_launch(&it.name, &it.mark);
            place.push((std::mem::take(&mut it.image), std::mem::take(&mut it.beside)));
            false
        });
        self.launch.due = due;
        if marked {
            self.vfs.flush(&mut self.effects);
        }
        for (image, mut beside) in place {
            let Some(last) = beside.pop() else { continue };
            for program in beside {
                // clone: the image's bytes, once for a second path beside it
                self.place_start(&program, image.clone());
            }
            self.place_start(&last, image);
        }
        for program in wants {
            self.launch_want(program);
        }
    }

    /// The start file beside the program at `program` placed holding `image`: its first step
    /// asked for; each next one as the one before it completes ([`Daemon::start_placed`]).
    fn place_start(&mut self, program: &str, image: Vec<u8>) {
        self.launch.places += 1;
        let number = self.launch.places;
        if number >= PLACES_MOST {
            tracing::error!(target: "blaze", "{program}: its start file is not placed: past 2^32 places in one daemon");
            return;
        }
        let placed = Placed {
            number,
            file: blaze_certify::slot::start_file(program),
            placing: Placing::new(program, image),
        };
        self.launch.placing.push(placed);
        self.place_step(self.launch.placing.len() - 1);
    }

    /// The next step of the place at `at` asked for of the driver; at its end, or where its
    /// path is no name the driver takes, the place is taken out (the latter said by name).
    fn place_step(&mut self, at: usize) {
        let placed = &mut self.launch.placing[at];
        let tag = Tag(super::PLACES + placed.number);
        let name = |path: &str| FileName::new(path.trim_start_matches('/'));
        let effect = match placed.placing.step() {
            Place::Write { to, bytes } => name(&to).map(|file| Effect::WriteUnsynced { file, bytes, tag }),
            Place::Sync(file) => name(&file).map(|file| Effect::Sync { file, tag }),
            // The rename that waits for the drive: a directory's sync does not make an unsynced
            // rename durable, so a crash after the place could otherwise undo it.
            Place::Rename { from, to } => name(&from).zip(name(&to)).map(|(from, to)| Effect::Rename { from, to, tag }),
            Place::SyncDir(dir) => name(&dir).map(|dir| Effect::SyncDir { dir, tag }),
            Place::Done => {
                self.launch.placing.swap_remove(at);
                return;
            }
        };
        match effect {
            Some(effect) => self.effects.push(effect),
            None => {
                let placed = self.launch.placing.swap_remove(at);
                tracing::error!(target: "blaze", "the start file {} is not placed: its path is no file name the driver takes", placed.file);
            }
        }
    }

    /// A step of a start file's place completed ([`super::PLACES`]): the next asked for, or,
    /// where it failed, the place ended and said at error naming the file. The mark stays: the
    /// program's next start restores from the store, and a launcher with no start file asks.
    pub(super) fn start_placed(&mut self, completion: Completion) {
        let number = completion.tag.0 - super::PLACES;
        let Some(at) = self.launch.placing.iter().position(|it| it.number == number) else { return };
        if let Done::Failed(error) = &completion.done {
            let placed = self.launch.placing.swap_remove(at);
            tracing::error!(target: "blaze", "the start file {} is not placed: {error:?}", placed.file);
            return;
        }
        self.place_step(at);
    }

    /// `name`'s mark set to `mark` unless it is already; whether the journal records it. A
    /// mark the journal refuses is said, naming it: the next start builds the program.
    fn mark_launch(&mut self, name: &str, mark: &[u8]) -> bool {
        if self.vfs.marked(name) == Some(mark) {
            return false;
        }
        match self.vfs.mark(name, mark) {
            Ok(_) => true,
            Err(error) => {
                tracing::error!(target: "blaze", "the journal refused the start image mark {name:?}: {error:?}");
                false
            }
        }
    }

    /// A first call of `program` waits for a front end no task holds: the build this daemon
    /// last answered for it, sent as its own; with none, or while it stops, the need
    /// ends by name ([`Steps::launch_built`]).
    fn launch_want(&mut self, program: String) {
        if self.stopping.is_some() {
            // alloc: the reason
            let why = "the daemon is stopping: run `blaze run` again once it has".to_owned();
            self.steps.launch_built(&program, Err(why));
            return;
        }
        if self.launch.sent.iter().any(|it| it.1 == program) {
            return;
        }
        if !self.launch.programs.iter().any(|it| it.0 == program) {
            // A launcher that started from its start file with no client's build of it here:
            // the request of the newest slot this daemon wrote for it, as the client's
            // command line would have asked it.
            match self.slot_request(&program) {
                Ok(Some(request)) => {
                    // clone: the path, once a program first built for a slot's request
                    self.launch.programs.push((program.clone(), request));
                }
                Ok(None) => {
                    // alloc: the reason
                    let why = "no `blaze run` of it reached this daemon since it started: run `blaze run`".to_owned();
                    self.steps.launch_built(&program, Err(why));
                    return;
                }
                Err(why) => {
                    self.steps.launch_built(&program, Err(why));
                    return;
                }
            }
        }
        let request = self.launch.programs.iter().find(|it| it.0 == program).expect("found or pushed above").1.clone(); // clone: the request, once a build for a first call
        self.launch.next += 1;
        let id = self.launch.next;
        let span = tracing::info_span!(
            target: "blaze",
            "launch: the daemon builds a program for a first call",
            program = %program,
            request = id,
        );
        self.launch.sent.push((id, program, None, span));
        self.to_session(Arrived::Request { from: RequestRef { conn: CONN, id }, request });
    }

    /// The build a slot this daemon wrote asks for, whose answer has a launcher at `program`:
    /// its frame, `(word, targets)` in the daemon's CBOR, made the client's `Request::Build`
    /// of the checkout its cargo line names. `None` where no slot of this daemon's names it.
    fn slot_request(&self, program: &str) -> Result<Option<Request>, String> {
        let Some(frame) = self.jobs.slot_request_of(program) else { return Ok(None) };
        let (_, targets): (String, blaze_api::Targets) = blaze_protocol::cbor::from_slice(frame)
            .map_err(|error| format!("{program}: the request of its slot does not decode: {error}"))?; // alloc: an error
        let Some(cargo) = targets.cargo.as_ref() else {
            // alloc: an error
            return Err(format!("{program}: the request of its slot names no checkout"));
        };
        // clone: the checkout's root, once a request
        let view = blaze_api::ViewRef::Head(cargo.root.clone());
        Ok(Some(Request::Build { view, targets }))
    }

    /// A reply to one of the daemon's own runs ([`CONN`]): at its `Done`, the launch host is
    /// told how it ended, and the need it was for is served by the task after its build or
    /// ended by name.
    pub(super) fn launch_heard(&mut self, reply: &Reply) {
        let Some(at) = self.launch.sent.iter().position(|it| it.0 == reply.to.id) else { return };
        match &reply.response {
            Response::Error(error) => {
                // clone: the error's text, once a failed build
                self.launch.sent[at].2 = Some(error.what.clone());
            }
            Response::Done { ok } => {
                let (_, program, error, _span) = self.launch.sent.swap_remove(at);
                let built = match (ok, error) {
                    (true, _) => Ok(()),
                    (false, Some(why)) => Err(why),
                    // alloc: the reason
                    (false, None) => Err("it ended not ok, with no error said".to_owned()),
                };
                self.steps.launch_built(&program, built);
            }
            _ => {}
        }
    }

    /// A client's request read: each build (`blaze run`'s among them) is kept until its
    /// `Done`, and a connection that is gone takes its builds with it.
    pub(super) fn launch_arrived(&mut self, arrived: &Arrived) {
        match arrived {
            Arrived::Request { from, request: request @ Request::Build { .. } } => {
                // clone: once a build, what the daemon sends itself for its program's first calls
                self.launch.runs.push((*from, request.clone()));
            }
            Arrived::Request { .. } => {}
            Arrived::Gone { conn } => self.launch.runs.retain(|it| it.0.conn != *conn),
        }
    }

    /// A reply to a client's request: a build's `Built` makes it the build last answered for
    /// each program it built; its `Done` forgets it.
    pub(super) fn launch_replied(&mut self, reply: &Reply) {
        let Some(at) = self.launch.runs.iter().position(|it| it.0 == reply.to) else { return };
        match &reply.response {
            Response::Built { artifacts, .. } => {
                for path in artifacts.iter().filter_map(|it| it.executable.as_deref()) {
                    self.launch.programs.retain(|it| it.0 != path);
                    // clone: the build, once a program it built
                    let request = self.launch.runs[at].1.clone();
                    self.launch.programs.push((path.to_owned(), request));
                }
            }
            Response::Done { .. } => {
                self.launch.runs.swap_remove(at);
            }
            _ => {}
        }
    }
}
