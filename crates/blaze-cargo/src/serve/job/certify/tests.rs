//! The daemon's driver of the request machine, against models: the code a run executes held
//! in no file (the driver's own [`outputs_of`] and blaze-cargo's [`CodeHolder`] and
//! [`HeldCode`]), and a build's own looks at the disk outside the views (the driver's
//! [`looked_reads`] and [`outside_now`], blaze-usercode's disk, the job's `native_shared`).
//! Moved with the machine's other tests from `serve/job/request/tests.rs` when the machine
//! went to blaze-certify (2026-11-12); these stay here because each drives the daemon's own
//! types and functions, which a crate with no compiler in it does not hold. The machine
//! against its model alone is blaze-certify's `request/tests.rs`.

use blaze_certify::testing::{GRANULE_NS, Shelf, stamp_at, the_base};
use blaze_core::Stamp;
use blaze_core::sim::{Faults, Sim};
use blaze_ids::ContentId;
use blaze_record::Digest;
use blaze_usercode::trace::Inputs as _;
use blaze_vfs::outside::{Look, LookKind, Looks};

use super::certificate::{self, Evidence, LAUNCHER, MEMORY, Read};
use super::request::{Ask, Built, How, Next, PLANT, Plant, Request, Told, Why};
use super::{AtPath, looked_reads, looked_twice, outputs_of, outside_now};
use crate::serve::work::{CodeHolder, HeldCode};

/// The daemon of a model: its store, and what it knows beside it.
struct Daemon {
    shelf: Shelf,
    evidence: Evidence,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Seen {
    Hit,
    Miss(Why),
}

fn node(path: &str) -> Read {
    Read::Node(path.to_owned())
}

// The code a run executes, held in no file (certificate.md; found 2026-11-12 by the
// measurement's "there and back": a launcher's file is the same bytes at every build, so a
// certificate of files alone hit on v=1 after v=2 and the run printed v=2's 57).


/// The program's path: its file, and the key of every holder's program.
const PROGRAM: &str = "/c/target/debug/app";

/// What a run of the program prints for source value `v`: `src/main.rs` prints
/// `(1..=10).sum() + v`.
fn printed_for(v: u8) -> u32 {
    55 + u32::from(v)
}

#[derive(Clone, Copy, Debug)]
enum CodeOp {
    /// `src/main.rs` written with this value.
    Source(u8),
    Request,
    /// The daemon restarts: the store stays; the holder's code and the evidence go.
    Restart,
    /// The program's file put back by hand to what a build of this value placed there.
    FileBack(u8),
    /// `src/main.rs` written with this value, and the next build, where the holder holds
    /// code, patches it in place: a code memory then holds the patch
    /// ([`Program::patch_of`]), and the file at the path is left as it is.
    Patch(u8),
    /// The daemon restarts, and the launch host restores its program from the image a mark
    /// names: of the build of this value, which may be older than the last build (the newer
    /// image was not durable when the daemon stopped), so the holder's code is stale.
    RestartFrom(u8),
}

/// A checkout of one program whose code a run takes from `holder`, not from its file.
struct Program {
    holder: CodeHolder,
    source: u8,
    file: Option<(ContentId, Stamp)>,
    /// The identity of the build whose code `holder` holds ([`HeldCode::Is`]).
    held: Option<ContentId>,
    /// The next build patches the code held ([`CodeOp::Patch`]).
    patch: bool,
    clock_ns: u64,
    /// The identity of the code the last build gave the holder: what its certificate's
    /// record names.
    certified: Option<ContentId>,
    /// The planted bug of [`the_oracle_catches_a_holder_that_answers_the_certificates_record`]:
    /// the look at the holder answers the last certificate's record, whatever it holds.
    answers_the_record: bool,
}

impl Program {
    fn new(holder: CodeHolder) -> Program {
        Program {
            holder,
            source: 0,
            file: None,
            held: None,
            patch: false,
            clock_ns: 10 * GRANULE_NS,
            certified: None,
            answers_the_record: false,
        }
    }

    /// The launcher: one file for every build of every program.
    fn launcher() -> ContentId {
        ContentId::of("test launcher", b"blaze-launcher")
    }

    /// The dev executable a build of source `v` links: its file, and its memory's.
    fn image(v: u8) -> ContentId {
        ContentId::of("test image", &[v])
    }

    /// The code a patch to source `v` leaves in a code memory: in no file.
    fn patch_of(v: u8) -> ContentId {
        ContentId::of("test patch", &[v])
    }

    /// The identity of the code a build of source `v` gives the holder: the unit's record key
    /// (the launch host), the file its memory holds (a code memory).
    fn code(&self, v: u8) -> ContentId {
        match self.holder {
            CodeHolder::Launcher => ContentId::of("test record", &[v]),
            CodeHolder::Memory => Program::image(v),
        }
    }

    /// What a build of source `v` places at the program's path.
    fn file_of(&self, v: u8) -> ContentId {
        match self.holder {
            CodeHolder::Launcher => Program::launcher(),
            CodeHolder::Memory => Program::image(v),
        }
    }

    fn stamp(&self) -> Stamp {
        stamp_at(self.clock_ns)
    }

    fn node(&self) -> Digest {
        Digest::Is(ContentId::of("test file", &[self.source]))
    }

    fn look(&self, ask: &Ask) -> Result<Told, String> {
        Ok(match ask {
            Ask::Read { read: Read::Node(path), .. } if path == "src/main.rs" => Told::Read(self.node()),
            Ask::Read { read, .. } => return Err(format!("asked for {read:?}, which no build reads")),
            // The driver's look at a holder ([`certify`]'s `code_told`): what it holds, in place.
            Ask::Content { base, .. } if certificate::holder_of(*base).is_some() => {
                let held = match self.answers_the_record {
                    true => self.certified,
                    false => self.held,
                };
                Told::Content(held.filter(|_| certificate::holder_of(*base) == Some(self.holder)))
            }
            Ask::Stamp { base, .. } if certificate::holder_of(*base).is_some() => {
                return Err("a holder was stamped".to_owned());
            }
            Ask::Stamp { .. } => Told::Stamp { stamp: self.file.map(|it| it.1), looked_ns: self.clock_ns },
            Ask::Content { .. } => Told::Content(self.file.map(|it| it.0)),
        })
    }

    /// A build: the launcher's file is placed by the program's first build in this daemon and
    /// left as it is by every later one; a dev executable's file is its memory's. The holder
    /// takes the build's code. What it hands the machine, its outputs named by the driver's
    /// own [`outputs_of`].
    fn build(&mut self) -> Result<Built, String> {
        self.clock_ns += 1;
        let first = self.held.is_none();
        let patch = std::mem::take(&mut self.patch) && !first && self.holder == CodeHolder::Memory;
        if patch {
            self.held = Some(Program::patch_of(self.source));
        } else {
            if first || self.holder == CodeHolder::Memory {
                self.file = Some((self.file_of(self.source), self.stamp()));
            }
            self.held = Some(self.code(self.source));
        }
        self.certified = self.held;
        let (file, held, holder, asked_ns) = (self.file, self.held, self.holder, self.clock_ns);
        let outputs = outputs_of(
            [PROGRAM],
            |_| match file {
                Some((content, stamp)) => AtPath::Placed { content, stamp: Some((stamp, asked_ns)) },
                None => AtPath::Nothing,
            },
            |of, _| held.filter(|_| of == holder).map(HeldCode::Is),
        )?;
        Ok(Built {
            nodes: vec!["src/main.rs".to_owned()],
            valued: Vec::new(),
            outputs,
            units: Vec::new(),
            said: b"Finished".to_vec(),
            untraced: Vec::new(),
        })
    }

    /// A run of the program: the code its holder holds, whatever file is at its path. A code
    /// memory that holds nothing (a daemon started since) is made at the launch from the
    /// file at the path, and holds that file's code from then on (`DevRuns::launch`); a
    /// launch host that holds nothing has nothing to run.
    fn run(&mut self) -> Result<u32, String> {
        if self.held.is_none() && self.holder == CodeHolder::Memory {
            self.held = self.file.map(|it| it.0);
        }
        let held = self.held.ok_or("the daemon holds no code of the program: the run stops")?;
        (0..=u8::MAX)
            .find(|v| self.code(*v) == held || (self.holder == CodeHolder::Memory && Program::patch_of(*v) == held))
            .map(printed_for)
            .ok_or_else(|| "the holder holds code of no source".to_owned())
    }
}

/// One request of `program` driven to its end, every ask answered in the seed's order.
fn code_request(program: &mut Program, daemon: &mut Daemon, sim: &mut Sim) -> Result<Seen, String> {
    let mut machine = Request::new(the_base(), false, Vec::new());
    let mut missed = None;
    for _ in 0..64 {
        match machine.step(&daemon.shelf, &mut daemon.evidence).map_err(|why| why.to_string())? {
            Next::Asks(mut asks) => {
                sim.rng().shuffle(&mut asks);
                for ask in &asks {
                    machine.told(ask, program.look(ask)?).map_err(|why| why.to_string())?;
                }
            }
            Next::Waiting { on, left } => return Err(format!("{on}: waits for {left} with everything told")),
            Next::Hit(_) => return Ok(Seen::Hit),
            Next::Miss(miss) => {
                machine.built(program.build()?).map_err(|why| why.to_string())?;
                missed = Some(miss.why);
            }
            Next::Written { .. } => return Ok(Seen::Miss(missed.ok_or("written with no miss before it")?)),
            Next::Unwritten(why) => return Err(format!("a build that answered left no record: {why}")),
        }
    }
    Err("the request did not end in 64 steps".to_owned())
}

/// Runs `ops` on a program held by `holder`, with `plant`; after every request the program
/// is run, and the oracle is that it prints its source's value. What each request came to.
fn run_code(holder: CodeHolder, ops: &[CodeOp], seed: u64, plant: Plant) -> Result<Vec<Seen>, String> {
    PLANT.set(plant);
    let ran = run_code_planted(holder, ops, seed, false);
    PLANT.set(Plant::None);
    ran
}

/// `ops` run as [`run_code`] does, with the machine unplanted; `answers_the_record` plants
/// [`Program::answers_the_record`].
fn run_code_planted(holder: CodeHolder, ops: &[CodeOp], seed: u64, answers_the_record: bool) -> Result<Vec<Seen>, String> {
    let mut sim = Sim::new(seed, Faults::default());
    let mut program = Program::new(holder);
    program.answers_the_record = answers_the_record;
    let mut daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    let mut seen = Vec::new();
    for (n, op) in ops.iter().enumerate() {
        // Every stamp is steady by the next op: a stamp vouches where evidence holds it.
        program.clock_ns += 3 * GRANULE_NS;
        match *op {
            CodeOp::Source(v) => program.source = v,
            CodeOp::Restart => {
                program.held = None;
                daemon.evidence = Evidence::default();
            }
            CodeOp::RestartFrom(v) => {
                program.held = Some(program.code(v));
                daemon.evidence = Evidence::default();
            }
            CodeOp::FileBack(v) => program.file = Some((program.file_of(v), program.stamp())),
            CodeOp::Patch(v) => (program.source, program.patch) = (v, true),
            CodeOp::Request => {
                seen.push(code_request(&mut program, &mut daemon, &mut sim).map_err(|why| format!("step {n}: {why}"))?);
                let printed = program.run().map_err(|why| format!("step {n}: {why}"))?;
                let want = printed_for(program.source);
                if printed != want {
                    return Err(format!(
                        "step {n}: the run printed {printed}, and its source prints {want}: it ran another build's code"
                    ));
                }
            }
        }
    }
    Ok(seen)
}

/// There and back, and on: v=1, v=2, v=1 (the measurement's case), v=1 again, v=2, the
/// file put back by hand to v=1's then v=1, and a restart.
const THERE_AND_BACK: [CodeOp; 14] = [
    CodeOp::Source(1),
    CodeOp::Request,
    CodeOp::Source(2),
    CodeOp::Request,
    CodeOp::Source(1),
    CodeOp::Request,
    CodeOp::Request,
    CodeOp::Source(2),
    CodeOp::Request,
    CodeOp::FileBack(1),
    CodeOp::Source(1),
    CodeOp::Request,
    CodeOp::Restart,
    CodeOp::Request,
];

fn code_miss(base: u8, how: How) -> Seen {
    Seen::Miss(Why::Output { base, path: PROGRAM.to_owned(), how })
}

/// The invariant: a run after a request executes the code of the request's source, for a
/// launcher's program, whose file is the same bytes at every build, and for a dev
/// executable, whose run maps the daemon's code memory over its file; under every order of
/// the driver's answers. A request whose holder holds another build's code, or none, misses
/// by name and builds; one whose holder holds its build's code hits.
#[test]
fn a_run_after_a_request_executes_the_code_of_its_source() {
    let moved = Seen::Miss(Why::Moved(vec![node("src/main.rs")]));
    let launcher = [
        Seen::Miss(Why::Unfiled),
        moved.clone(),
        code_miss(LAUNCHER, How::Changed),
        Seen::Hit,
        code_miss(LAUNCHER, How::Changed),
        code_miss(LAUNCHER, How::Changed),
        code_miss(LAUNCHER, How::Gone),
    ];
    let memory = [
        Seen::Miss(Why::Unfiled),
        moved,
        code_miss(MEMORY, How::Changed),
        Seen::Hit,
        // The memory's file is placed at the path too: its stamp vouches for v=1's bytes.
        code_miss(0, How::Changed),
        code_miss(MEMORY, How::Changed),
        // A started daemon's memory holds nothing: the file at the path is v=1's, as the
        // record's memory is, so the launch makes the memory the build left.
        Seen::Hit,
    ];
    for seed in 0..16 {
        let seen = run_code(CodeHolder::Launcher, &THERE_AND_BACK, seed, Plant::None).unwrap();
        assert_eq!(seen, launcher, "seed {seed}");
        let seen = run_code(CodeHolder::Memory, &THERE_AND_BACK, seed, Plant::None).unwrap();
        assert_eq!(seen, memory, "seed {seed}");
    }
}

/// The control (rule 23): with the holder's entry left out of the record, the launcher's
/// v=1 after v=2 hits and runs v=2's code (step 5, the measurement's 57), and the dev
/// executable whose file was put back by hand hits and runs its memory's v=2 (step 11).
#[test]
fn the_oracle_catches_the_held_code_left_out_of_the_record() {
    for seed in 0..16 {
        let why = run_code(CodeHolder::Launcher, &THERE_AND_BACK, seed, Plant::LeavesCodeOut)
            .expect_err("the oracle missed the launcher's code left out");
        assert!(why.starts_with("step 5: the run printed 57, and its source prints 56"), "{why}");
        let why = run_code(CodeHolder::Memory, &THERE_AND_BACK, seed, Plant::LeavesCodeOut)
            .expect_err("the oracle missed the memory's code left out");
        assert!(why.starts_with("step 11: the run printed 57, and its source prints 56"), "{why}");
    }
}

/// v=1, then v=2, then a restart whose launch host restores the image of v=1's build (the
/// mark of v=2's image was not durable), then a request of v=2.
const STALE_IMAGE: [CodeOp; 6] = [
    CodeOp::Source(1),
    CodeOp::Request,
    CodeOp::Source(2),
    CodeOp::Request,
    CodeOp::RestartFrom(1),
    CodeOp::Request,
];

/// The invariant: after a restart, a request is answered with no build (a hit, 0 jobs) only
/// where the record the launch host restored is its certificate's; a host restored from a
/// stale image (the mark of an older build) misses by name on the launcher's output and
/// builds, and the run prints its source's value.
#[test]
fn a_restart_from_a_stale_image_misses_on_the_launcher() {
    for seed in 0..16 {
        let seen = run_code_planted(CodeHolder::Launcher, &STALE_IMAGE, seed, false).unwrap();
        assert_eq!(seen.last(), Some(&code_miss(LAUNCHER, How::Changed)), "seed {seed}: {seen:?}");
    }
}

/// The control (rule 23; 7c-launched-restart.md's second): a holder that answers the
/// certificate's record for any mark hits on the stale image with no build, and the run,
/// of v=1's code, prints 56 for v=2's 57; the oracle catches it.
#[test]
fn the_oracle_catches_a_holder_that_answers_the_certificates_record() {
    for seed in 0..16 {
        let why = run_code_planted(CodeHolder::Launcher, &STALE_IMAGE, seed, true)
            .expect_err("the oracle missed a holder that answers the certificate's record");
        assert!(why.starts_with("step 5: the run printed 56, and its source prints 57"), "{why}");
    }
}

/// The invariant: a code memory that holds nothing (a daemon started since the build) is
/// what a launch makes from the file at the program's path, so a request whose record's
/// memory is that file's code hits with no build, and its run prints its source's value;
/// one whose record's memory is code in no file (a patch) misses by name and builds.
/// Control: the plant that takes any file for the memory hits on the patched build's record
/// after a start, and the run, made from the unpatched file, prints v=1's 56 for v=2.
#[test]
fn a_memory_output_with_no_holder_is_the_file_at_its_path() {
    let restarted = [CodeOp::Source(1), CodeOp::Request, CodeOp::Restart, CodeOp::Request];
    let patched =
        [CodeOp::Source(1), CodeOp::Request, CodeOp::Patch(2), CodeOp::Request, CodeOp::Restart, CodeOp::Request];
    let moved = Seen::Miss(Why::Moved(vec![node("src/main.rs")]));
    for seed in 0..16 {
        let seen = run_code(CodeHolder::Memory, &restarted, seed, Plant::None).unwrap();
        assert_eq!(seen, [Seen::Miss(Why::Unfiled), Seen::Hit], "seed {seed}");
        let seen = run_code(CodeHolder::Memory, &patched, seed, Plant::None).unwrap();
        assert_eq!(seen, [Seen::Miss(Why::Unfiled), moved.clone(), code_miss(MEMORY, How::Gone)], "seed {seed}");
        let why = run_code(CodeHolder::Memory, &patched, seed, Plant::TakesAnyFileForTheMemory)
            .expect_err("the oracle missed a patched memory taken for its file");
        assert!(why.starts_with("step 5: the run printed 56, and its source prints 57"), "{why}");
    }
}

/// The invariant: a launch host that holds nothing (a daemon started since the build) has
/// no code of the program, whatever file is at its path (the launcher's, the same at every
/// build), so the request misses by name and builds. Control: the same request with the
/// host's code kept hits, so the miss is the start's.
#[test]
fn a_launcher_output_with_no_holder_misses() {
    let restarted = [CodeOp::Source(1), CodeOp::Request, CodeOp::Restart, CodeOp::Request];
    let kept = [CodeOp::Source(1), CodeOp::Request, CodeOp::Request];
    for seed in 0..16 {
        let seen = run_code(CodeHolder::Launcher, &restarted, seed, Plant::None).unwrap();
        assert_eq!(seen, [Seen::Miss(Why::Unfiled), code_miss(LAUNCHER, How::Gone)], "seed {seed}");
        let seen = run_code(CodeHolder::Launcher, &kept, seed, Plant::None).unwrap();
        assert_eq!(seen, [Seen::Miss(Why::Unfiled), Seen::Hit], "the control, seed {seed}");
    }
}

/// A holder that holds code of no build blaze can name leaves no certificate, by name: never
/// a record with the entry left out.
#[test]
fn code_of_no_named_build_leaves_no_certificate() {
    let why = outputs_of(
        [PROGRAM],
        |_| AtPath::Placed { content: Program::launcher(), stamp: Some((stamp_at(0), 0)) },
        |holder, _| (holder == CodeHolder::Launcher).then_some(HeldCode::Unnamed),
    )
    .expect_err("a certificate was written of code no build is named for");
    assert!(why.contains(PROGRAM) && why.contains("no build blaze can name"), "{why}");
}

/// The invariant (one writer, D45): an output another request placed since the build answered
/// leaves no certificate, named with both keys; never an entry naming that request's file as
/// this answer's, never one left out.
#[test]
fn an_output_placed_by_another_request_since_the_answer_leaves_no_certificate() {
    let (left, holds) = (ContentId([1; 32]), ContentId([2; 32]));
    let why = outputs_of([PROGRAM], |_| AtPath::Replaced { left, holds: Some(holds) }, |_, _| None)
        .expect_err("a certificate named another request's file as this answer's");
    assert!(why.contains(PROGRAM) && why.contains(&format!("{left:x}")) && why.contains(&format!("{holds:x}")), "{why}");
    let why = outputs_of([PROGRAM], |_| AtPath::Replaced { left, holds: None }, |_, _| None)
        .expect_err("a certificate named a path whose placement failed");
    assert!(why.contains(PROGRAM) && why.contains("nothing"), "{why}");
}

/// The invariant (one writer, D45): a certificate's output is the answer's only where the
/// jobs' record holds the answer's key there; another key, or a placement in flight, is
/// another request's file, and the request goes to its job. With no record (a start, a path
/// this daemon never placed) the disk alone is the evidence. Each case, and the planted
/// shortcut that takes any record for the answer's.
#[test]
fn a_certificates_output_is_the_answers_only_where_its_record_holds_the_answers_key() {
    use super::{Recorded, placed_elsewhere};
    let (wanted, other) = (ContentId([1; 32]), ContentId([2; 32]));
    let cases = [
        (Recorded::Holds(wanted), false),
        (Recorded::Holds(other), true),
        (Recorded::Placing, true),
        (Recorded::Unrecorded, false),
    ];
    for (recorded, elsewhere) in cases {
        assert_eq!(placed_elsewhere(recorded, wanted), elsewhere, "{recorded:?}");
    }
    // The planted shortcut: it vouches whatever the record holds, and the case of another
    // key's record is the one the oracle sees it pass.
    let vouches_any = |_: Recorded, _: ContentId| false;
    assert!(
        cases.iter().any(|(recorded, elsewhere)| vouches_any(*recorded, wanted) != *elsewhere),
        "the planted shortcut answers as the check does in every case: the cases prove nothing"
    );
}

// A build's own looks at the disk outside the views, in its certificate: each look a read of
// its own, at what it saw, asked again by the driver's real look ([`certify`]'s
// `outside_now`) on a real directory.


/// The files the build looks at: past 64, the count a process-wide record of looks once
/// stopped naming paths at, so no bound on looks is in the way of a certificate.
const LOOKED_FILES: usize = 65;

/// A directory outside every view: [`LOOKED_FILES`] headers, a link (looked at as an entry,
/// not followed) and a directory the build lists.
struct Outside {
    dir: tempfile::TempDir,
}

impl Outside {
    fn new() -> Outside {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for n in 0..LOOKED_FILES {
            std::fs::write(dir.path().join(format!("f{n}.h")), [n as u8]).expect("a header written");
        }
        std::fs::create_dir(dir.path().join("listed")).expect("a directory made");
        std::fs::write(dir.path().join("listed").join("a"), b"a").expect("a file written");
        std::os::unix::fs::symlink("f0.h", dir.path().join("entry")).expect("a link made");
        Outside { dir }
    }

    fn file(&self, n: usize) -> std::path::PathBuf {
        self.dir.path().join(format!("f{n}.h"))
    }

    /// The build: one look at each header, at the link and at the directory, through the
    /// disk as a task looks (blaze-usercode's `Now`), into a `Looks` of its own. Its looks
    /// as the certificate's write takes them ([`Looks::all`]).
    fn looks(&self) -> Vec<Look> {
        let looks = Looks::new();
        let disk = blaze_usercode::inputs::Disk::new(Vec::new(), &looks);
        let mut now = disk.now();
        for n in 0..LOOKED_FILES {
            now.file(&self.file(n)).expect("a header read");
        }
        now.entry(&self.dir.path().join("entry")).expect("the link read");
        now.dir(&self.dir.path().join("listed")).expect("the directory listed");
        looks.all()
    }

    /// Moves what a look of `kind` saw: a header's content, the link's target, a name in
    /// the directory.
    fn change(&self, kind: LookKind) {
        match kind {
            LookKind::File => std::fs::write(self.file(40), b"moved").expect("a header written"),
            LookKind::Entry => {
                let link = self.dir.path().join("entry");
                std::fs::remove_file(&link).expect("the link removed");
                std::os::unix::fs::symlink("f1.h", &link).expect("the link made again");
            }
            LookKind::Listing => {
                std::fs::write(self.dir.path().join("listed").join("b"), b"b").expect("a file written")
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Looked {
    Hit,
    /// A miss for this reason, then a build whose certificate was written.
    Written(Why),
}

/// One request of a build whose reads are its own looks at `outside` ([`looked_reads`]),
/// every ask answered in the seed's order: a node by its one value, any other read by a
/// look now ([`outside_now`]).
fn looked_request(outside: &Outside, daemon: &mut Daemon, sim: &mut Sim) -> Result<Looked, String> {
    valued_request(&|| looked_reads(&outside.looks()), daemon, sim)
}

/// One request whose build, when it builds, reads what `valued` says a fresh build's looks
/// are, every ask answered in the seed's order as [`looked_request`]'s.
fn valued_request(
    valued: &dyn Fn() -> Result<Vec<(Read, Digest)>, String>,
    daemon: &mut Daemon,
    sim: &mut Sim,
) -> Result<Looked, String> {
    let mut machine = Request::new(the_base(), false, Vec::new());
    let mut missed = None;
    for _ in 0..64 {
        match machine.step(&daemon.shelf, &mut daemon.evidence).map_err(|why| why.to_string())? {
            Next::Asks(mut asks) => {
                sim.rng().shuffle(&mut asks);
                for ask in &asks {
                    let told = match ask {
                        Ask::Read { read: Read::Node(_), .. } => Told::Read(Digest::Is(ContentId::of("test file", &[0]))),
                        Ask::Read { read, .. } => Told::Read(outside_now(read)),
                        other => return Err(format!("asked {other:?} of a build that placed nothing")),
                    };
                    machine.told(ask, told).map_err(|why| why.to_string())?;
                }
            }
            Next::Waiting { on, left } => return Err(format!("{on}: waits for {left} with everything told")),
            Next::Hit(_) => return Ok(Looked::Hit),
            Next::Miss(miss) => {
                missed = Some(miss.why);
                machine
                    .built(Built {
                        nodes: vec!["src/main.rs".to_owned()],
                        valued: valued()?,
                        outputs: Vec::new(),
                        units: Vec::new(),
                        said: b"Finished".to_vec(),
                        untraced: Vec::new(),
                    })
                    .map_err(|why| why.to_string())?;
            }
            Next::Written { .. } => {
                return Ok(Looked::Written(missed.ok_or("a record was written with no miss before it")?));
            }
            Next::Unwritten(why) => return Err(format!("no certificate was written: {why}")),
        }
    }
    Err("the request did not end in 64 steps".to_owned())
}

/// A certificate written with `plant` in the write, then what a request says after a look
/// of `kind` moved. The oracle: a hit is given only while every look a fresh build makes
/// says what the certified build's did.
fn after_a_change(kind: LookKind, plant: Plant) -> Result<Looked, String> {
    let outside = Outside::new();
    let mut sim = Sim::new(1, Faults::default());
    let mut daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    PLANT.set(plant);
    let written = looked_request(&outside, &mut daemon, &mut sim);
    PLANT.set(Plant::None);
    match written? {
        Looked::Written(_) => {}
        other => return Err(format!("the first request came to {other:?}, not a certificate written")),
    }
    let before = outside.looks();
    outside.change(kind);
    let seen = looked_request(&outside, &mut daemon, &mut sim)?;
    if seen == Looked::Hit && outside.looks() != before {
        return Err(format!("a hit after a {kind:?} look a fresh build makes moved"));
    }
    Ok(seen)
}

/// The invariant: a build that looked at the disk outside every view, at many files, an
/// entry and a listing, leaves a certificate that holds each look as a read of its own, and
/// the next request with nothing moved is a hit.
#[test]
fn a_build_that_looked_at_the_disk_is_certified_and_answered_again() {
    let outside = Outside::new();
    let looks = outside.looks();
    let of = |kind| looks.iter().filter(|it| it.kind == kind).count();
    assert_eq!((of(LookKind::File), of(LookKind::Entry), of(LookKind::Listing)), (LOOKED_FILES, 1, 1));
    assert_eq!(looked_reads(&looks).expect("the looks as reads").len(), LOOKED_FILES + 2);
    let mut sim = Sim::new(1, Faults::default());
    let mut daemon = Daemon { shelf: Shelf::new(), evidence: Evidence::default() };
    assert!(matches!(looked_request(&outside, &mut daemon, &mut sim), Ok(Looked::Written(_))));
    assert!(!daemon.shelf.held.borrow().is_empty());
    assert_eq!(looked_request(&outside, &mut daemon, &mut sim), Ok(Looked::Hit));
}

/// The invariant: a certified build's look at a file outside every view that moved is a
/// miss, never a hit.
#[test]
fn a_moved_file_the_build_looked_at_is_a_miss() {
    assert!(matches!(after_a_change(LookKind::File, Plant::None), Ok(Looked::Written(_))));
}

/// The control: with the build's file looks left out of the certificate, the moved file is
/// a hit, which the oracle refuses.
#[test]
fn the_oracle_catches_the_file_looks_left_out_of_the_record() {
    let why = after_a_change(LookKind::File, Plant::DropsLooked(LookKind::File)).expect_err("the oracle missed it");
    assert!(why.contains("a hit after a File look"), "{why}");
}

/// The invariant: a certified build's look at an entry (a link, not followed) that moved is
/// a miss (and a build, certified anew), never a hit.
#[test]
fn a_moved_entry_the_build_looked_at_is_a_miss() {
    assert!(matches!(after_a_change(LookKind::Entry, Plant::None), Ok(Looked::Written(_))));
}

/// The control: with the build's entry looks left out of the certificate, the moved link is
/// a hit, which the oracle refuses.
#[test]
fn the_oracle_catches_the_entry_looks_left_out_of_the_record() {
    let why = after_a_change(LookKind::Entry, Plant::DropsLooked(LookKind::Entry)).expect_err("the oracle missed it");
    assert!(why.contains("a hit after a Entry look"), "{why}");
}

/// The invariant: a certified build's listing of a directory outside every view whose
/// names moved is a miss, never a hit.
#[test]
fn a_moved_listing_the_build_looked_at_is_a_miss() {
    assert!(matches!(after_a_change(LookKind::Listing, Plant::None), Ok(Looked::Written(_))));
}

/// The control: with the build's listings left out of the certificate, the directory with a
/// new name is a hit, which the oracle refuses.
#[test]
fn the_oracle_catches_the_listings_left_out_of_the_record() {
    let why =
        after_a_change(LookKind::Listing, Plant::DropsLooked(LookKind::Listing)).expect_err("the oracle missed it");
    assert!(why.contains("a hit after a Listing look"), "{why}");
}

/// The invariant: a build that saw one path at two values leaves no certificate, and the
/// refusal names the path and both callers that saw it differ; one value is no refusal.
#[test]
fn a_path_seen_at_two_values_names_both_callers() {
    let looks = Looks::new();
    let path = std::path::Path::new("/usr/include/z.h");
    looks.push(path, LookKind::File, Some([1; 32]));
    let first = std::panic::Location::caller();
    assert_eq!(looked_twice(&looks.all()), None, "one value is no refusal");
    looks.push(path, LookKind::File, Some([2; 32]));
    let all = looks.all();
    assert_eq!(all.len(), 2);
    let why = looked_twice(&all).expect("a path seen at two values is refused");
    assert!(why.contains("/usr/include/z.h") && why.contains("at two values"), "{why}");
    for look in &all {
        assert!(why.contains(&look.by.to_string()), "{why} names {}", look.by);
    }
    assert_eq!(first.file(), all[0].by.file());
}

// A task two builds share: a pinned native input's layout, one task a digest for every job
// asking it (`job::native_shared`), its look at the layout outside every view carried in its
// result. Each build's certificate, then the layout moved.

/// The builds of two jobs that share one layout task, `ran` (the build whose job posted it)
/// and `joined` (one that joined it), each with a certificate of its own in a daemon of its
/// own; then the layout moves (a link to elsewhere where its directory was) and each asks
/// again. The oracle: a hit is given to neither, as a fresh build's task looks again and sees
/// the link. What each second request came to, in that order.
fn shared_layout_moved(plant: Plant) -> Result<[Looked; 2], String> {
    let outside = tempfile::tempdir().expect("a temporary directory");
    let laid = outside.path().join("laid");
    std::fs::create_dir(&laid).expect("a layout made");
    let (ran, joined) = (Looks::new(), Looks::new());
    // The task, run once for both builds: its result as the loop keeps it.
    PLANT.set(plant);
    let shared = super::super::native_shared(laid.clone(), &ran);
    PLANT.set(Plant::None);
    let shared = shared?;
    // Each build's job takes the one result, the one whose post ran it first.
    for build in [&ran, &joined] {
        assert_eq!(shared.take(build), &laid);
    }
    let mut sim = Sim::new(1, Faults::default());
    let mut daemons = [
        Daemon { shelf: Shelf::new(), evidence: Evidence::default() },
        Daemon { shelf: Shelf::new(), evidence: Evidence::default() },
    ];
    for (build, daemon) in [&ran, &joined].into_iter().zip(daemons.iter_mut()) {
        match valued_request(&|| looked_reads(&build.all()), daemon, &mut sim)? {
            Looked::Written(_) => {}
            other => return Err(format!("the first request came to {other:?}, not a certificate written")),
        }
    }
    // What a fresh build rests on: the task run again, and its looks.
    let fresh = || super::super::native_shared(laid.clone(), &Looks::none()).map(|it| it.looks().to_vec());
    let before = fresh()?;
    std::fs::remove_dir(&laid).expect("the layout removed");
    std::os::unix::fs::symlink(outside.path(), &laid).expect("a link made where the layout was");
    let after = fresh()?;
    assert_ne!(before, after, "the task's look sees the layout move");
    let mut seen = Vec::new();
    for (daemon, name) in daemons.iter_mut().zip(["ran", "joined"]) {
        let it = valued_request(&|| looked_reads(&after), daemon, &mut sim)?;
        if it == Looked::Hit {
            return Err(format!("a hit for the build that {name} the shared task after the layout it rests on moved"));
        }
        seen.push(it);
    }
    let [first, second]: [Looked; 2] = seen.try_into().map_err(|_| "two requests".to_owned())?;
    Ok([first, second])
}

/// The invariant: a build that takes a shared task's result holds the task's looks whichever
/// build's job ran it, so after what the task looked at moves, the certificate of the build
/// that joined it misses as the one that ran it does, never a hit.
#[test]
fn both_builds_sharing_a_task_miss_once_what_it_looked_at_moved() {
    let seen = shared_layout_moved(Plant::None).expect("no hit after the move");
    assert!(seen.iter().all(|it| matches!(it, Looked::Written(_))), "{seen:?}");
}

/// The control: with the task's looks given to the build whose job ran it alone, the build
/// that joined it is answered by a hit after the layout moved, which the oracle refuses.
#[test]
fn the_oracle_catches_a_shared_task_whose_looks_go_to_its_runner_alone() {
    let why = shared_layout_moved(Plant::LooksToRunner).expect_err("the oracle missed it");
    assert!(why.contains("a hit for the build that joined"), "{why}");
}
