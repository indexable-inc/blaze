//! Tests of c_tasks.rs: a script's deferred C requests run as object and archive tasks.
//!
//! The compiler is a fake ([`Fake`]): an object is bytes derived from what it is given (its
//! cooked argv, the source's text and each file the source says it reads), so a key that
//! misses an input shows as an object that should have changed and did not. The store is
//! written once per key (other bytes at a held key are refused, as blaze-cli's), the file
//! area is in memory, and today's files are a list ([`World`]).
//!
//! (a) three requests and an `ar`: a `run` runs 3 objects and 1 archive, a `check` between
//!     runs nothing and leaves the script's record as it was, a second `run` runs 0;
//! (b) one source edited: 1 object and the archive; a header edited: the objects that read it;
//! (c) control: the compiler's identity changed: all 3 objects again;
//! (d) a failing compile is one error naming the crate and the source; another crate's tasks
//!     are run and recorded;
//! (e) two checkouts at different host folders give equal archive bytes; control: a compiler
//!     that lets the host folder into its output makes them differ.

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::c_tasks::{
    Area, CCompiler, CStore, CompilerKind, Compile, Compiled, FactError, FileRef, GuestRoots, PlanError, Refused, RequestWhy, StoreFailed, TaskError, Today, Todays, Toolchain, plan,
    run_crate, script_demand, write_archive,
};
use crate::instr::SearchKind;
use crate::outputs::{CRequest, CState, Outputs, OutputsRef, Snap, snap_digest};

const OUT: &str = "/target/debug/build/foo-1/out";

fn roots() -> GuestRoots<'static> {
    GuestRoots { package: "/package", out: OUT, builds: "/target/debug/build", deps: "/deps", sysroot: "/sdk" }
}

fn own<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    String::from_str_in(text, alloc)
}

fn joined<'a>(argv: &[&str], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for (at, arg) in argv.iter().enumerate() {
        if at > 0 {
            out.push('\0');
        }
        out.push_str(arg);
    }
    out
}

/// The flag check's source as the script wrote it into `OUT_DIR`.
const FLAG_CHECK_C: &str = "int main(void) { return 0; }\n";

/// A script's record as `check` leaves it: cc-rs's probes, a flag check (an assumption: no
/// `ar` names its object), three compiles, an `ar cq` and an `ar s`; `static=foo` in `OUT_DIR`.
fn script_record(alloc: Alloc<'_>) -> AVec<'_, u8> {
    let mut outputs = Outputs::new_in(OUT, alloc);
    let o = |name: &str| {
        let mut path = own(OUT, alloc);
        path.push('/');
        path.push_str(name);
        path
    };
    let (detect, check, check_c) = (o("detect_compiler_family.c"), o("flag_check"), o("flag_check.c"));
    let (a, b, c, lib) = (o("a.o"), o("b.o"), o("c.o"), o("libfoo.a"));
    let (detect, check, check_c) = (detect.as_str(), check.as_str(), check_c.as_str());
    let (a, b, c, lib) = (a.as_str(), b.as_str(), c.as_str(), lib.as_str());
    let requests: [&[&str]; 7] = [
        &["cc", "-E", detect],
        &["cc", "-Wno-unused-command-line-argument", "-Wlogical-op", "-o", check, "-c", check_c],
        &["cc", "-O2", "-I", "/package/include", "-fPIC", "-o", a, "-c", "/package/src/a.c"],
        &["cc", "-O2", "-I", "/package/include", "-fPIC", "-o", b, "-c", "/package/src/b.c"],
        &["cc", "-O2", "-I", "/package/include", "-fPIC", "-o", c, "-c", "/package/src/c.S"],
        &["ar", "cqD", lib, a, b, c],
        &["ar", "sD", lib],
    ];
    for argv in requests {
        outputs.c_requests.push(CRequest { argv: joined(argv, alloc), state: 0 });
    }
    // The one `OUT_DIR` state the requests saw: the flag check's source, which the script then
    // removed (it is in no file of the record).
    let digest = snap_digest(FLAG_CHECK_C.as_bytes());
    outputs.hold_c_bytes(digest, FLAG_CHECK_C.as_bytes(), alloc);
    let mut entries = Vec::new_in(alloc);
    entries.push((own("flag_check.c", alloc), Snap::Held(digest)));
    outputs.c_states.push(CState { entries });
    outputs.link_libs.push(own("static=foo", alloc));
    outputs.link_search.push((SearchKind::Native, own(OUT, alloc)));
    let mut bytes = Vec::new_in(alloc);
    outputs.encode(&mut bytes);
    bytes
}

/// Today's files of one checkout: guest paths and their bytes; `host` is the folder the
/// checkout is at, which only a compiler that leaks it ([`Fake::leaks_host`]) could see.
struct World {
    host: &'static str,
    files: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>,
}

impl World {
    fn new(host: &'static str) -> World {
        let files = [
            ("/package/include/h.h", "int h(void);\n"),
            ("/package/include/g.h", "int g(void);\n"),
            ("/package/src/a.c", "read /package/include/h.h\nint a(void) { return 1; }\n"),
            ("/package/src/b.c", "read /package/include/g.h\nint b(void) { return 2; }\n"),
            ("/package/src/c.S", "ret\n"),
        ];
        let mut files: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)> = files.iter().map(|(p, t)| (p.to_string(), t.as_bytes().to_vec())).collect();
        // The record's one `OUT_DIR` state, as a driver lays it.
        files.push((format!("{OUT}/flag_check.c"), FLAG_CHECK_C.as_bytes().to_vec()));
        World { host, files }
    }

    fn set(&mut self, path: &str, text: &str) {
        match self.files.iter_mut().find(|(p, _)| p == path) {
            Some((_, bytes)) => *bytes = text.as_bytes().to_vec(),
            None => self.files.push((path.to_string(), text.as_bytes().to_vec())),
        }
    }

    fn bytes(&self, path: &str) -> Option<&[u8]> {
        self.files.iter().find(|(p, _)| p == path).map(|(_, b)| b.as_slice())
    }
}

impl Today for World {
    fn file(&self, path: &str) -> Result<Option<[u8; 32]>, FactError> {
        Ok(self.bytes(path).map(|bytes| ContentId::of("c_tasks test file", bytes).0))
    }

    fn listing(&self, path: &str) -> Result<Option<[u8; 32]>, FactError> {
        let mut names: std::vec::Vec<&str> = self.files.iter().filter_map(|(p, _)| p.strip_prefix(path).filter(|rest| rest.starts_with('/'))).collect();
        if names.is_empty() {
            return Ok(None);
        }
        names.sort_unstable();
        Ok(Some(ContentId::of("c_tasks test listing", names.join("\n").as_bytes()).0))
    }
}

/// The record's one state is laid into the world itself.
impl Todays for World {
    fn of(&self, _state: u32) -> &dyn Today {
        self
    }
}

/// The fake compiler: the object is the cooked argv, the source's text and each read file's
/// text; a source holding `error` fails with clang's form of message.
struct Fake<'w> {
    world: &'w World,
    area: &'w core::cell::RefCell<Files>,
    ran: u32,
    /// Control of (e): the object holds the checkout's host folder (a compile with no prefix map).
    leaks_host: bool,
}

impl CCompiler for Fake<'_> {
    const KIND: CompilerKind = CompilerKind::ShelfClang;

    fn compile<'a>(&mut self, unit: &Compile<'_, '_>, alloc: Alloc<'a>) -> Result<Compiled<'a>, Refused<'a>> {
        self.ran += 1;
        let Some(source) = self.world.bytes(unit.source) else {
            return Err(Refused { words: own("the fake compiler found no source", alloc) });
        };
        let text = core::str::from_utf8(source).expect("c_tasks test: sources are text");
        if text.contains("error") {
            let mut first_lines = own(unit.source, alloc);
            first_lines.push_str(":1:1: error: expected expression");
            return Ok(Compiled::Failed { status: 1, first_lines });
        }
        let mut object = std::vec::Vec::new();
        object.extend_from_slice(b"FAKEOBJ\n");
        if self.leaks_host {
            object.extend_from_slice(self.world.host.as_bytes());
        }
        for arg in unit.args.iter().chain([&unit.source, &unit.output]) {
            object.extend_from_slice(arg.as_bytes());
            object.push(0);
        }
        object.extend_from_slice(source);
        let mut reads = Vec::new_in(alloc);
        reads.push(own(unit.source, alloc));
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("read ") {
                object.extend_from_slice(self.world.bytes(path).expect("c_tasks test: a read file is there"));
                reads.push(own(path, alloc));
            }
        }
        let file = self.area.borrow_mut().put(&object).map_err(|_| Refused { words: own("the area refused", alloc) })?;
        Ok(Compiled::Made { object: file, reads })
    }
}

/// The file area: bytes by a digest standing in for sha256.
#[derive(Default)]
struct Files {
    held: std::vec::Vec<(FileRef, std::vec::Vec<u8>)>,
}

impl Files {
    fn put(&mut self, bytes: &[u8]) -> Result<FileRef, StoreFailed> {
        let file = FileRef { sha256: ContentId::of("c_tasks test area", bytes).0, len: bytes.len() as u64 };
        if !self.held.iter().any(|(f, _)| *f == file) {
            self.held.push((file, bytes.to_vec()));
        }
        Ok(file)
    }

    fn bytes(&self, file: FileRef) -> Option<&[u8]> {
        self.held.iter().find(|(f, _)| *f == file).map(|(_, b)| b.as_slice())
    }
}

/// The area as the seam sees it (the fake compiler holds the same files).
struct AreaOf<'f>(&'f core::cell::RefCell<Files>);

impl Area for AreaOf<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<FileRef, StoreFailed> {
        self.0.borrow_mut().put(bytes)
    }

    fn read(&self, file: FileRef, into: &mut AVec<'_, u8>) -> Result<(), StoreFailed> {
        into.clear();
        into.extend_from_slice(self.0.borrow().bytes(file).ok_or(StoreFailed)?);
        Ok(())
    }
}

/// The store: written once per key, equal bytes again left as they are, other bytes refused.
#[derive(Default)]
struct Store {
    held: std::vec::Vec<(ContentId, std::vec::Vec<u8>)>,
}

impl CStore for Store {
    fn get(&mut self, key: ContentId, into: &mut AVec<'_, u8>) -> Result<bool, StoreFailed> {
        into.clear();
        match self.held.iter().find(|(k, _)| *k == key) {
            Some((_, bytes)) => {
                into.extend_from_slice(bytes);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), StoreFailed> {
        match self.held.iter().find(|(k, _)| *k == key) {
            Some((_, held)) if held == bytes => Ok(()),
            Some(_) => Err(StoreFailed),
            None => {
                self.held.push((key, bytes.to_vec()));
                Ok(())
            }
        }
    }
}

fn toolchain(compiler: u8) -> Toolchain<'static> {
    Toolchain { kind: CompilerKind::ShelfClang, compiler: [compiler; 32], sdk: b"26.1 25B78", target: "aarch64-apple-darwin" }
}

/// One `run`'s C tasks of crate `package` against `world`: (objects run, archives run, the
/// archive's bytes).
fn run(package: &str, record: &[u8], world: &World, files: &core::cell::RefCell<Files>, store: &mut Store, compiler: u8, leaks_host: bool) -> (u32, u32, std::vec::Vec<u8>) {
    let alloc = blaze_own::testing::heap().alloc();
    let outputs = OutputsRef::open(record).expect("c_tasks test: the record opens");
    let demand = script_demand(&outputs, alloc);
    let demand: std::vec::Vec<&str> = demand.iter().map(|d| d.as_str()).collect();
    let mut fake = Fake { world, area: files, ran: 0, leaks_host };
    let ran = run_crate(package, &outputs, &roots(), &demand, &toolchain(compiler), &mut fake, world, &mut AreaOf(files), store, alloc).unwrap_or_else(|e| panic!("{package}: {e:?}"));
    assert_eq!(fake.ran, ran.objects_ran, "every miss compiled once, nothing else");
    assert_eq!(ran.made.len(), 1);
    assert_eq!(ran.made[0].name, "libfoo.a");
    let bytes = files.borrow().bytes(ran.made[0].file).expect("c_tasks test: the archive is in the area").to_vec();
    (ran.objects_ran, ran.archives_ran, bytes)
}

#[test]
fn plan_counts_the_requests() {
    let alloc = blaze_own::testing::heap().alloc();
    let record = script_record(alloc);
    let outputs = OutputsRef::open(&record).expect("opens");
    let plan = plan(&outputs, &roots(), &["libfoo.a"], alloc).expect("plans");
    assert_eq!((plan.counts.requests, plan.counts.objects, plan.counts.archives, plan.counts.unused, plan.counts.assumed, plan.counts.probes), (7, 3, 1, 0, 1, 2));
    assert_eq!((plan.planned, plan.units.len()), (3, 4), "the archive's three objects, then the flag check");
    assert_eq!(plan.units[3].source, format!("{OUT}/flag_check.c"));
    // Nothing demanded: no archive's objects; the assumption is still the script's.
    let none = crate::c_tasks::plan(&outputs, &roots(), &[], alloc).expect("plans");
    assert_eq!((none.counts.objects, none.counts.archives, none.counts.unused, none.counts.assumed), (0, 0, 3, 1));
}

#[test]
fn a_run_then_a_check_then_a_run() {
    let alloc = blaze_own::testing::heap().alloc();
    let record = script_record(alloc);
    let before = record.to_vec();
    let world = World::new("/Users/a/tempo");
    let files = core::cell::RefCell::new(Files::default());
    let mut store = Store::default();
    let (objects, archives, first) = run("foo", &record, &world, &files, &mut store, 1, false);
    assert_eq!((objects, archives), (4, 1), "a first run: three objects, the flag check it assumed, and the archive");
    // `check` asks for no object and no archive: it opens the script's record and stops. The
    // store gains nothing and the record is the bytes it was.
    let held = store.held.len();
    let _ = OutputsRef::open(&record).expect("check opens the record");
    assert_eq!(store.held.len(), held);
    assert_eq!(record.as_slice(), before.as_slice(), "the script's record is unchanged");
    let (objects, archives, second) = run("foo", &record, &world, &files, &mut store, 1, false);
    assert_eq!((objects, archives), (0, 0), "a second run runs nothing");
    assert_eq!(first, second);
}

#[test]
fn an_edit_runs_what_read_it() {
    let alloc = blaze_own::testing::heap().alloc();
    let record = script_record(alloc);
    let mut world = World::new("/Users/a/tempo");
    let files = core::cell::RefCell::new(Files::default());
    let mut store = Store::default();
    let (_, _, first) = run("foo", &record, &world, &files, &mut store, 1, false);
    world.set("/package/src/b.c", "read /package/include/g.h\nint b(void) { return 3; }\n");
    let (objects, archives, edited) = run("foo", &record, &world, &files, &mut store, 1, false);
    assert_eq!((objects, archives), (1, 1), "one source edited: its object and the archive");
    assert_ne!(first, edited);
    // A header's text: only the object that read it (a.c reads h.h).
    world.set("/package/include/h.h", "int h(void); /* edited */\n");
    assert_eq!(run("foo", &record, &world, &files, &mut store, 1, false).0, 1);
    // The include structure: b.c now reads h.h too; a new path set, b's object alone.
    world.set("/package/src/b.c", "read /package/include/g.h\nread /package/include/h.h\nint b(void) { return 3; }\n");
    assert_eq!(run("foo", &record, &world, &files, &mut store, 1, false).0, 1);
    // Back to the first text: every object's records are held, nothing runs.
    world.set("/package/src/b.c", "read /package/include/g.h\nint b(void) { return 3; }\n");
    world.set("/package/include/h.h", "int h(void);\n");
    assert_eq!(run("foo", &record, &world, &files, &mut store, 1, false), (0, 0, edited));
    // A new file in a searched folder could shadow a header: every compile searching it runs.
    world.set("/package/include/new.h", "int n(void);\n");
    assert_eq!(run("foo", &record, &world, &files, &mut store, 1, false).0, 3);
}

#[test]
fn control_a_new_compiler_runs_every_object() {
    let alloc = blaze_own::testing::heap().alloc();
    let record = script_record(alloc);
    let world = World::new("/Users/a/tempo");
    let files = core::cell::RefCell::new(Files::default());
    let mut store = Store::default();
    run("foo", &record, &world, &files, &mut store, 1, false);
    let (objects, archives, _) = run("foo", &record, &world, &files, &mut store, 2, false);
    assert_eq!((objects, archives), (4, 0), "the compiler's identity is in every object's key");
    // The fake's objects do not hold the compiler's identity, so the archive's key (its
    // members' digests) is the first's: a real compiler's objects differ and so would it.
}

#[test]
fn a_failed_compile_names_the_crate_and_the_source() {
    let alloc = blaze_own::testing::heap().alloc();
    let record = script_record(alloc);
    let mut bad = World::new("/Users/a/tempo");
    bad.set("/package/src/b.c", "int b(void) { return error; }\n");
    let good = World::new("/Users/a/tempo");
    let files = core::cell::RefCell::new(Files::default());
    let mut store = Store::default();
    let outputs = OutputsRef::open(&record).expect("opens");
    let mut fake = Fake { world: &bad, area: &files, ran: 0, leaks_host: false };
    let failed = run_crate("bad-sys", &outputs, &roots(), &["libfoo.a"], &toolchain(1), &mut fake, &bad, &mut AreaOf(&files), &mut store, alloc);
    match failed {
        Err(TaskError::Compile { package, source, status, first_lines }) => {
            assert_eq!((package, source, status), ("bad-sys", "/package/src/b.c", 1));
            assert!(first_lines.as_str().contains("error: expected expression"), "{}", first_lines.as_str());
        }
        other => panic!("expected the compile's error, got {other:?}"),
    }
    // Another crate's tasks run and are recorded.
    let (objects, archives, _) = run("good-sys", &record, &good, &files, &mut store, 1, false);
    assert_eq!((objects, archives), (3, 1), "a.c's object was recorded before b.c failed; b, c and the flag check run now");
    assert_eq!(run("good-sys", &record, &good, &files, &mut store, 1, false).0, 0);
}

#[test]
fn two_checkouts_make_equal_bytes() {
    let alloc = blaze_own::testing::heap().alloc();
    let record = script_record(alloc);
    let (here, there) = (World::new("/Users/a/tempo"), World::new("/Volumes/b/tempo-2"));
    let (files_here, files_there) = (core::cell::RefCell::new(Files::default()), core::cell::RefCell::new(Files::default()));
    let (mut store_here, mut store_there) = (Store::default(), Store::default());
    let (_, _, a) = run("foo", &record, &here, &files_here, &mut store_here, 1, false);
    let (_, _, b) = run("foo", &record, &there, &files_there, &mut store_there, 1, false);
    assert_eq!(a, b, "the cooked argv holds guest paths only");
    assert!(a.starts_with(b"!<arch>\n"));
    // Control: a compiler that lets the host folder in makes the archives differ.
    let (_, _, c) = run("foo", &record, &here, &core::cell::RefCell::new(Files::default()), &mut Store::default(), 1, true);
    let (_, _, d) = run("foo", &record, &there, &core::cell::RefCell::new(Files::default()), &mut Store::default(), 1, true);
    assert_ne!(c, d);
}

#[test]
fn a_cl_style_record_is_refused_by_name() {
    // ring's record as today's `check` made it (cc-rs took the compiler for MSVC: `cc -?` was
    // answered with exit 0).
    let alloc = blaze_own::testing::heap().alloc();
    let mut outputs = Outputs::new_in(OUT, alloc);
    let mut obj = own(OUT, alloc);
    obj.push_str("/x-curve25519.o");
    let mut fo = own("-Fo", alloc);
    fo.push_str(obj.as_str());
    outputs.c_requests.push(CRequest { argv: joined(&["cc", "-nologo", "-MD", "-Z7", "-Brepro", "-I", "/package/include", fo.as_str(), "-c", "/package/crypto/curve25519/curve25519.c"], alloc), state: 0 });
    let mut lib = own(OUT, alloc);
    lib.push_str("/libring_core.a");
    outputs.c_requests.push(CRequest { argv: joined(&["ar", "cqD", lib.as_str(), obj.as_str()], alloc), state: 0 });
    outputs.c_states.push(CState { entries: Vec::new_in(alloc) });
    let mut bytes = Vec::new_in(alloc);
    outputs.encode(&mut bytes);
    let opened = OutputsRef::open(&bytes).expect("opens");
    match plan(&opened, &roots(), &["libring_core.a"], alloc) {
        Err(PlanError::Request { error, .. }) => assert_eq!((error.request, error.arg, error.why), (0, 1, RequestWhy::ClStyle)),
        other => panic!("expected the cl-style refusal, got {other:?}"),
    }
}

#[test]
fn the_archive_is_deterministic_and_reads_as_bsd() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut one = Vec::new_in(alloc);
    let mut two = Vec::new_in(alloc);
    let members: [(&str, &[u8]); 2] = [("a.o", b"abc".as_slice()), ("a-long-member-name-x.o", b"0123".as_slice())];
    write_archive(&members, &mut one);
    write_archive(&members, &mut two);
    assert_eq!(one, two);
    assert!(one.starts_with(b"!<arch>\n#1/4 "));
    // The first member: a header at 8, the name padded so the data starts at 72 (a multiple of
    // 8), three bytes, one pad byte to the next even offset.
    let head = &one[8..68];
    assert_eq!(&head[58..60], b"`\n");
    assert_eq!(core::str::from_utf8(&head[48..58]).expect("text").trim_end(), "7");
    assert_eq!(&one[68..72], b"a.o\0");
    assert_eq!(&one[72..76], b"abc\n");
    // The second's header at 76, its data at a multiple of 8 too.
    let second = core::str::from_utf8(&one[76..92]).expect("text").trim_end();
    let padded: usize = second.strip_prefix("#1/").expect("a BSD long name").parse().expect("a length");
    assert_eq!((76 + 60 + padded) % 8, 0);
    assert_eq!(&one[76 + 60 + padded..76 + 60 + padded + 4], b"0123");
    // Its date, uid and gid are 0 and its mode 644.
    assert_eq!(core::str::from_utf8(&head[16..48]).expect("text").split_whitespace().collect::<std::vec::Vec<_>>(), ["0", "0", "0", "644"]);
}

// ---- The C inputs: each compile reads `OUT_DIR` as it was at its spawn ----
//
// The script side (blaze-proc-macro spawn_route_tests.rs, `a_deferred_compile_keeps_*`) holds
// that the record captures each state; these hold that the tasks compile each state's bytes.

/// The version of `x.c` the script wrote first, and the one it overwrote it with.
const X_A: &str = "int x(void) { return 0xA; }\n";
const X_B: &str = "int x(void) { return 0xB; }\n";

/// Today's facts of each state: one [`World`] per state, `OUT_DIR` laid as the state holds it.
struct Worlds(std::vec::Vec<World>);

impl Worlds {
    /// Each state of `outputs` laid by [`crate::c_tasks::laid_state`].
    fn laid(outputs: &OutputsRef<'_>) -> Worlds {
        let alloc = blaze_own::testing::heap().alloc();
        let mut worlds = std::vec::Vec::new();
        for state in 0..outputs.c_state_count() {
            let laid = crate::c_tasks::laid_state(outputs, state, alloc).unwrap_or_else(|e| panic!("c_tasks test: {e}"));
            let files = laid.iter().filter_map(|(name, bytes)| bytes.map(|b| (format!("{OUT}/{name}"), b.to_vec()))).collect();
            worlds.push(World { host: "/Users/a/tempo", files });
        }
        Worlds(worlds)
    }

    /// CONTROL: every state read as the `OUT_DIR` the script left at its end (the record's
    /// files), as a driver that reads `OUT_DIR` at run time would.
    fn final_only(outputs: &OutputsRef<'_>) -> Worlds {
        let worlds = (0..outputs.c_state_count())
            .map(|_| World { host: "/Users/a/tempo", files: outputs.files().map(|(name, bytes)| (format!("{OUT}/{name}"), bytes.to_vec())).collect() })
            .collect();
        Worlds(worlds)
    }
}

impl Todays for Worlds {
    fn of(&self, state: u32) -> &dyn Today {
        &self.0[state as usize]
    }
}

/// The fake compiler over the world of each unit's state.
struct PerState<'w> {
    worlds: &'w Worlds,
    area: &'w core::cell::RefCell<Files>,
}

impl CCompiler for PerState<'_> {
    const KIND: CompilerKind = CompilerKind::ShelfClang;

    fn compile<'a>(&mut self, unit: &Compile<'_, '_>, alloc: Alloc<'a>) -> Result<Compiled<'a>, Refused<'a>> {
        Fake { world: &self.worlds.0[unit.state as usize], area: self.area, ran: 0, leaks_host: false }.compile(unit, alloc)
    }
}

/// A record whose requests are `requests` (argv, state), its states `states` (name, the bytes
/// the state held, `None` a final file), its files `finals`; `static=x` from `OUT_DIR`.
fn state_record<'a>(requests: &[(&[&str], u32)], states: &[&[(&str, Option<&str>)]], finals: &[(&str, &str)], alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut outputs = Outputs::new_in(OUT, alloc);
    for (argv, state) in requests {
        outputs.c_requests.push(CRequest { argv: joined(argv, alloc), state: *state });
    }
    for entries in states {
        let mut held = Vec::new_in(alloc);
        for (name, bytes) in entries.iter() {
            let snap = match bytes {
                Some(text) => {
                    let digest = snap_digest(text.as_bytes());
                    outputs.hold_c_bytes(digest, text.as_bytes(), alloc);
                    Snap::Held(digest)
                }
                None => Snap::Final,
            };
            held.push((own(name, alloc), snap));
        }
        outputs.c_states.push(CState { entries: held });
    }
    for (name, text) in finals {
        outputs.add_file(name, text.as_bytes(), crate::outputs::FileLimits { bytes: 1 << 20, files: 16 }, alloc).expect("c_tasks test: a final file");
    }
    outputs.link_libs.push(own("static=x", alloc));
    outputs.link_search.push((SearchKind::Native, own(OUT, alloc)));
    let mut bytes = Vec::new_in(alloc);
    outputs.encode(&mut bytes);
    bytes
}

/// One run of `record`'s tasks over `worlds`: the archive's bytes, or the task error's words.
fn run_states(record: &[u8], worlds: &Worlds) -> Result<std::vec::Vec<u8>, std::string::String> {
    let alloc = blaze_own::testing::heap().alloc();
    let outputs = OutputsRef::open(record).expect("c_tasks test: the record opens");
    let demand = script_demand(&outputs, alloc);
    let demand: std::vec::Vec<&str> = demand.iter().map(|d| d.as_str()).collect();
    let files = core::cell::RefCell::new(Files::default());
    let mut compiler = PerState { worlds, area: &files };
    let mut store = Store::default();
    let ran = run_crate("x", &outputs, &roots(), &demand, &toolchain(1), &mut compiler, worlds, &mut AreaOf(&files), &mut store, alloc).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ran.made.len(), 1, "libx.a");
    let bytes = files.borrow().bytes(ran.made[0].file).expect("c_tasks test: the archive is in the area").to_vec();
    Ok(bytes)
}

fn times(hay: &[u8], needle: &str) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle.as_bytes()).count()
}

/// (capture a) The script writes `gone.c` into `OUT_DIR`, defers its compile and the `ar`,
/// then deletes it (aws-lc-sys's `remove_dir_all` of its check folder): the run compiles the
/// state's bytes. CONTROL: reading `OUT_DIR` as the script left it finds no source, a typed
/// `NoSource` naming it.
#[test]
fn a_source_the_script_deleted_is_compiled_from_its_state() {
    let alloc = blaze_own::testing::heap().alloc();
    let (gone, obj, lib) = (format!("{OUT}/gone.c"), format!("{OUT}/gone.o"), format!("{OUT}/libx.a"));
    let (gone, obj, lib) = (gone.as_str(), obj.as_str(), lib.as_str());
    let requests: [(&[&str], u32); 2] = [(&["cc", "-O2", "-o", obj, "-c", gone], 0), (&["ar", "cqD", lib, obj], 0)];
    let record = state_record(&requests, &[&[("gone.c", Some(X_A))]], &[], alloc);
    let outputs = OutputsRef::open(&record).expect("opens");
    let archive = run_states(&record, &Worlds::laid(&outputs)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(times(&archive, X_A), 1, "the deleted source's bytes, from its state");
    let control = run_states(&record, &Worlds::final_only(&outputs));
    let Err(words) = control else { panic!("CONTROL: with OUT_DIR read at run time the deleted source was compiled") };
    assert!(words.starts_with("NoSource") && words.contains("gone.c"), "CONTROL: {words}");
}

/// (capture b) The script writes `x.c` = A, defers its compile, rewrites `x.c` = B, defers
/// again (cc-rs reuses one file name per check): the archive holds an object of A and one of
/// B. The second state's `x.c` is the record's final file (`Snap::Final`), held once. CONTROL:
/// reading `OUT_DIR` at run time compiles B twice, and the property fails.
#[test]
fn a_source_the_script_rewrote_is_compiled_once_per_version() {
    let alloc = blaze_own::testing::heap().alloc();
    let (x, one, two, lib) = (format!("{OUT}/x.c"), format!("{OUT}/x1.o"), format!("{OUT}/x2.o"), format!("{OUT}/libx.a"));
    let (x, one, two, lib) = (x.as_str(), one.as_str(), two.as_str(), lib.as_str());
    let requests: [(&[&str], u32); 3] = [(&["cc", "-o", one, "-c", x], 0), (&["cc", "-o", two, "-c", x], 1), (&["ar", "cqD", lib, one, two], 1)];
    let record = state_record(&requests, &[&[("x.c", Some(X_A))], &[("x.c", None)]], &[("x.c", X_B)], alloc);
    let outputs = OutputsRef::open(&record).expect("opens");
    assert_eq!(outputs.c_snap_bytes("x.c", Snap::Final), Some(X_B.as_bytes()), "the final state is the record's own file");
    let archive = run_states(&record, &Worlds::laid(&outputs)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((times(&archive, X_A), times(&archive, X_B)), (1, 1), "one object of each version");
    let control = run_states(&record, &Worlds::final_only(&outputs)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((times(&control, X_A), times(&control, X_B)), (0, 2), "CONTROL: OUT_DIR read at run time gives B twice");
}

/// hidapi-rusb 1.3.3's compile as tempo's `run` met it (08:27): cc-rs passes the script's
/// relative paths, which clang resolves against the spawn's folder, the package. The record
/// holds them rooted there (`rooted`), and the request reads as an object of `/package` paths.
/// Flag values that are no path (`-D`, `-x`, `--target=`) and absolute paths are kept as
/// spelled. CONTROL: the argv as spawned is refused by `read` (a relative path is no guest
/// path), so the rooting is what makes it a task.
#[test]
fn a_relative_path_is_rooted_at_the_spawns_folder() {
    let alloc = blaze_own::testing::heap().alloc();
    let obj = format!("{OUT}/4a064686e6ebdd3f-hid.o");
    let spawned = [
        "cc", "-O0", "-DX=y/z", "-I", "etc/hidapi/hidapi", "-Isrc/./inc", "-x", "c", "--target=arm64-apple-macosx", "-o", obj.as_str(), "-c", "./etc/hidapi/mac/hid.c",
    ];
    let mut text = std::string::String::new();
    crate::c_tasks::rooted(&spawned, "/package", &mut text).expect("writes");
    let want = [
        "cc", "-O0", "-DX=y/z", "-I", "/package/etc/hidapi/hidapi", "-I/package/src/./inc", "-x", "c", "--target=arm64-apple-macosx", "-o", obj.as_str(), "-c", "/package/etc/hidapi/mac/hid.c",
    ];
    assert_eq!(text, want.join("\0"));
    match crate::c_tasks::read(0, 0, &text, &roots(), alloc) {
        Ok(crate::c_tasks::Request::Object(unit)) => {
            assert_eq!(unit.source, "/package/etc/hidapi/mac/hid.c");
            assert_eq!(unit.searched.as_slice(), &["/package/etc/hidapi/hidapi", "/package/src/./inc"]);
        }
        other => panic!("expected an object, got {other:?}"),
    }
    let mut ar = std::string::String::new();
    crate::c_tasks::rooted(&["ar", "cq", "libx.a", "a.o"], "/package/sub", &mut ar).expect("writes");
    assert_eq!(ar, "ar\0cq\0/package/sub/libx.a\0/package/sub/a.o");
    let as_spawned = spawned.join("\0");
    match crate::c_tasks::read(0, 0, &as_spawned, &roots(), alloc) {
        Err(error) => assert_eq!(error.why, RequestWhy::HostPath, "CONTROL"),
        other => panic!("CONTROL: the relative argv read as {other:?}"),
    }
}

/// The argument shapes tempo's records hold (the loop home's, 2026-11-21 08:55) read as clang
/// takes them: libusb1-sys's empty defines, aws-lc-sys's `-ffile-prefix-map=/package=`,
/// `--param ssp-buffer-size=4` and `--include=<prefix header>`; `--param`'s value is no path
/// to root. CONTROL: an empty value of a flag that needs one is refused, and the refusal names
/// the request's argv with the argument marked.
#[test]
fn tempos_argument_shapes_read_as_clang_takes_them() {
    let alloc = blaze_own::testing::heap().alloc();
    let obj = format!("{OUT}/285c3a87414af4c4-darwin_usb.o");
    let spawned = [
        "cc", "-DPRINTF_FORMAT(a, b)=", "-DDEFAULT_VISIBILITY=__attribute__((visibility(\"default\")))", "-ffile-prefix-map=/package=", "--param", "ssp-buffer-size=4",
        "--include=/package/generated-include/openssl/boringssl_prefix_symbols_asm.h", "-o", obj.as_str(), "-c", "libusb/os/darwin_usb.c",
    ];
    let mut text = std::string::String::new();
    crate::c_tasks::rooted(&spawned, "/package", &mut text).expect("writes");
    assert!(text.contains("\0--param\0ssp-buffer-size=4\0"), "`--param`'s value is kept as spelled: {text:?}");
    assert!(text.ends_with("\0-c\0/package/libusb/os/darwin_usb.c"), "the source is rooted: {text:?}");
    match crate::c_tasks::read(0, 0, &text, &roots(), alloc) {
        Ok(crate::c_tasks::Request::Object(unit)) => assert_eq!(unit.source, "/package/libusb/os/darwin_usb.c"),
        other => panic!("expected an object, got {other:?}"),
    }
    let missing = ["cc", "-mmacosx-version-min=", "-o", obj.as_str(), "-c", "/package/a.c"].join("\0");
    let error = match crate::c_tasks::read(0, 0, &missing, &roots(), alloc) {
        Err(error) => error,
        other => panic!("CONTROL: an empty deployment target read as {other:?}"),
    };
    assert_eq!((error.arg, error.why), (1, RequestWhy::EmptyValue), "CONTROL");
    let words = std::format!("{}", crate::c_tasks::RefusedRequest { error, text: &missing });
    assert!(words.starts_with("request 0, argument 1 `-mmacosx-version-min=`: its value is empty"), "{words}");
    assert!(words.ends_with(&std::format!("cc >>-mmacosx-version-min=<< -o {obj} -c /package/a.c")), "{words}");
}

/// The built record's key (c_tasks_built.rs `built_key`) moves with each input it names, and
/// with nothing else; a built record opens to the payload it was made of, and a deferred
/// script record (no magic) does not open as one.
#[test]
fn the_built_key_moves_with_each_input() {
    use crate::c_tasks_built::{Pinned, built_key, encode_built, open_built};
    let pinned = Pinned { source: [1; 32], files: [2; 32] };
    let dep = [("/deps/zstd", [6; 32])];
    let key = |out: u8, tc: &Toolchain<'_>, pinned: Pinned, demand: &[&str], deps: &[(&str, [u8; 32])], tree: Option<[u8; 32]>| built_key(ContentId([out; 32]), tc, pinned, demand, deps, tree);
    let base = key(9, &toolchain(7), pinned, &["static=foo"], &dep, None);
    assert_eq!(base, key(9, &toolchain(7), pinned, &["static=foo"], &dep, None), "a key from equal inputs is equal");
    let sdk = Toolchain { sdk: b"26.2 25C56", ..toolchain(7) };
    let tree = key(9, &toolchain(7), pinned, &["static=foo"], &dep, Some([4; 32]));
    let moved = [
        ("the script's out", key(8, &toolchain(7), pinned, &["static=foo"], &dep, None)),
        ("the compiler", key(9, &toolchain(6), pinned, &["static=foo"], &dep, None)),
        ("the SDK", key(9, &sdk, pinned, &["static=foo"], &dep, None)),
        ("the source", key(9, &toolchain(7), Pinned { source: [3; 32], ..pinned }, &["static=foo"], &dep, None)),
        ("the files", key(9, &toolchain(7), Pinned { files: [3; 32], ..pinned }, &["static=foo"], &dep, None)),
        ("the demand", key(9, &toolchain(7), pinned, &["static=foo", "static=bar"], &dep, None)),
        ("the demand's split", key(9, &toolchain(7), pinned, &["static=fo", "o"], &dep, None)),
        ("a dependency tree's digest", key(9, &toolchain(7), pinned, &["static=foo"], &[("/deps/zstd", [7; 32])], None)),
        ("a dependency tree's spelling", key(9, &toolchain(7), pinned, &["static=foo"], &[("/deps/lz4", [6; 32])], None)),
        ("no dependency tree", key(9, &toolchain(7), pinned, &["static=foo"], &[], None)),
        ("a tree's tools", tree),
    ];
    for (what, moved_key) in moved {
        assert_ne!(moved_key, base, "{what} is not in the built key");
    }
    assert_ne!(tree, key(9, &toolchain(7), pinned, &["static=foo"], &dep, Some([5; 32])), "a changed tool or OS build is another built record");
    let alloc = blaze_own::testing::heap().alloc();
    let record = script_record(alloc);
    let mut payload: AVec<'_, u8> = Vec::new_in(alloc);
    payload.push(1);
    payload.extend_from_slice(&record);
    let mut built: AVec<'_, u8> = Vec::new_in(alloc);
    encode_built(&payload, &mut built);
    assert_eq!(open_built(&built), Some(&payload[..]));
    assert_eq!(open_built(&payload), None, "CONTROL: a script payload is no built record");
}

/// Every `links` dependency's tree a request names is found (librocksdb-sys's
/// `-I/deps/zstd/zstd/lib`, a producer's `OUT_DIR` whole or after `=`), each once; the
/// package, the script's own `OUT_DIR` and a word that only holds a guest spelling are not.
#[test]
fn the_dependency_trees_a_request_names_are_found() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut outputs = Outputs::new_in(OUT, alloc);
    let requests: [&[&str]; 2] = [
        &["cc", "-I/deps/zstd/zstd/lib", "-I", "/target/debug/build/lz4-sys-2/out/include", "-o", "/target/debug/build/foo-1/out/a.o", "-c", "/package/a.c"],
        &["cc", "-iquote=/deps/zstd/x", "-DP=a/deps/no", "-I/package/i", "-o", "/target/debug/build/foo-1/out/b.o", "-c", "/deps/lz4/b.c"],
    ];
    for argv in requests {
        outputs.c_requests.push(CRequest { argv: joined(argv, alloc), state: 0 });
    }
    outputs.c_states.push(CState { entries: Vec::new_in(alloc) });
    let mut bytes = Vec::new_in(alloc);
    outputs.encode(&mut bytes);
    let opened = OutputsRef::open(&bytes).unwrap_or_else(|e| panic!("{e:?}"));
    let found = crate::c_tasks::dep_roots(&opened, &roots(), alloc);
    assert_eq!(found.as_slice(), ["/deps/lz4", "/deps/zstd", "/target/debug/build/lz4-sys-2/out"]);
    assert_eq!(roots().root_of("/deps"), None, "CONTROL: `/deps` itself names no producer");
}
