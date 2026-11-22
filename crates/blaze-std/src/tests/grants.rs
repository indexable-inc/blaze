//! `main`'s parameters as grants (crate::grant) and the run spec (crate::spec), on the
//! simulated world: programs of several signatures run through [`launch_in`] with the body
//! `#[bstd::main]` makes ([`crate::launch_body!`]), each refusal with its own words, and the
//! spec's round trip to equal bytes and an equal digest.

use std::{cell::Cell, string::String as StdString};

use blaze_own::{SharedAlloc, String, Vec};
use blaze_sim::{Config, Tape};

use super::{check, path, root};
use crate::{
    Plant,
    args::Args,
    clock::Clock,
    cx::Cx,
    entry::{Error, Status, launch_in},
    fs::{Dir, ReadOnly, ReadWrite},
    grant::{Grant, Kind, LaunchAlloc, Manifest, Param, Spec},
    io::Io,
    net::Net,
    out::Out,
    spec::{self, GrantName, Granted, Layer, Layers, Provider, RunLimits, RunSpec, Scope, Stream},
};

/// The allocator the tests' specs are in.
fn a() -> LaunchAlloc {
    SharedAlloc::global()
}

fn text(raw: &str) -> String<LaunchAlloc> {
    String::from_str_in(raw, a())
}

/// A spec with no grants, as a parent starts from.
fn bare() -> RunSpec<LaunchAlloc> {
    RunSpec::defaults(Vec::new_in(a()), Vec::new_in(a()), text("/work"), a())
}

fn granted(name: &str, scope: Scope<LaunchAlloc>) -> Granted<LaunchAlloc> {
    Granted { name: GrantName::new(name, a()).expect("a test's grant name"), scope, provider: None, layers: None }
}

fn dir_scope(at: &str, write: bool) -> Scope<LaunchAlloc> {
    Scope::Dir { path: text(at), write }
}

/// `spec` as file descriptor 3 holds it.
fn delivered(spec: &RunSpec<LaunchAlloc>) -> std::vec::Vec<u8> {
    spec::delivery(spec, a()).as_slice().to_vec()
}

/// What the simulated world's standard output holds.
fn written(io: &Io) -> StdString {
    StdString::from_utf8(io.script(|host| host.out.clone()).expect("a simulated world")).expect("UTF-8")
}

/// A simulated world whose `fixtures/hello.txt` holds `hello from data`.
fn with_fixture(seed: u64) -> Io {
    let io = Io::sim(seed);
    let cx = root(&io);
    let fixtures = io.need_fs().expect("a file tree").open_dir(&cx, &path("fixtures")).expect("a folder");
    super::write_synced(&cx, &fixtures, &path("hello.txt"), b"hello from data").expect("the fixture is written");
    io
}

/// The manifest `#[bstd::main]` makes, for a test's program: `$name: $ty`, after `cx`.
macro_rules! manifest {
    ($($name:ident : $ty:ty),*) => {{
        static MANIFEST: Manifest = Manifest::new(&[$(Param::new(stringify!($name), <$ty as Grant>::KIND)),*]);
        &MANIFEST
    }};
}

// The programs, each a `main` of another shape.

fn streams(cx: &Cx, out: Out, err: Out, args: Args, _clock: Clock) -> Result<(), Error> {
    out.line(cx, "to out")?;
    err.line(cx, "to err")?;
    out.line(cx, args.len())?;
    Ok(())
}

thread_local! {
    /// Whether `cat` ran: a refusal starts no `main`.
    static RAN: Cell<bool> = const { Cell::new(false) };
}

/// The example's program: `data`'s `hello.txt` on standard output.
fn cat(cx: &Cx, out: Out, data: Dir<ReadOnly>) -> Result<(), Error> {
    RAN.with(|ran| ran.set(true));
    let mut into = cx.bytes();
    data.read_all(cx, &path("hello.txt"), &mut into)?;
    out.line(cx, core::str::from_utf8(into.as_slice()).map_err(|_| Error::new("hello.txt is not UTF-8"))?)?;
    Ok(())
}

fn own_spec(cx: &Cx, out: Out, spec: Spec) -> Result<(), Error> {
    let digest = spec.get().digest(a());
    out.line(cx, format_args!("{digest}"))?;
    Ok(())
}

fn writes(cx: &Cx, out: Out, cache: Dir<ReadWrite>, api: Net) -> Result<(), Error> {
    cache.publish_synced(cx, &path("made"), crate::fs::Content::bytes(b"made"))?;
    let _ = api;
    out.line(cx, "wrote")?;
    Ok(())
}

/// `cat` launched with `fd3`, on `io`: its status, what it said, and whether `main` ran.
fn launch_cat(io: &Io, fd3: Option<&[u8]>) -> (Status, StdString, bool) {
    RAN.with(|ran| ran.set(false));
    let mut said = StdString::new();
    let manifest = manifest!(out: Out, data: Dir<ReadOnly>);
    let status = launch_in(manifest, fd3, io.clone(), &mut said, crate::launch_body!(cat; out: Out, data: Dir<ReadOnly>));
    (status, said, RAN.with(Cell::get))
}

#[test]
fn every_shape_of_main_is_filled_and_runs() {
    let mut said = StdString::new();
    // What fills itself needs no spec; `out` and `err` are the two streams.
    let io = Io::sim(2);
    let manifest = manifest!(out: Out, err: Out, args: Args, clock: Clock);
    let body = crate::launch_body!(streams; out: Out, err: Out, args: Args, clock: Clock);
    assert_eq!(launch_in(manifest, None, io.clone(), &mut said, body), Status::OK);
    assert_eq!(written(&io), "to out\n0\n");
    assert_eq!(io.script(|host| host.err.clone()).expect("a simulated world"), b"to err\n");

    // A folder granted read only, read.
    let io = with_fixture(3);
    let mut spec = bare();
    spec.put_grant(granted("data", dir_scope("fixtures", false)));
    let (status, said, ran) = launch_cat(&io, Some(&delivered(&spec)));
    assert_eq!((status, said.as_str(), ran, written(&io).as_str()), (Status::OK, "", true, "hello from data\n"));

    // A folder granted read and write, handed to a read-only parameter: narrowed.
    let io = with_fixture(4);
    let mut spec = bare();
    spec.put_grant(granted("data", dir_scope("fixtures", true)));
    assert_eq!(launch_cat(&io, Some(&delivered(&spec))).0, Status::OK);

    // The program's own spec: the digest the parent computed.
    let io = Io::sim(5);
    let spec = bare();
    let mut said = StdString::new();
    let status = launch_in(manifest!(out: Out, spec: Spec), Some(&delivered(&spec)), io.clone(), &mut said, crate::launch_body!(own_spec; out: Out, spec: Spec));
    assert_eq!((status, said.as_str()), (Status::OK, ""));
    assert_eq!(written(&io), std::format!("{}\n", spec.digest(a())));

    // A read-write folder and one endpoint, both granted.
    let io = Io::sim(6);
    let mut spec = bare();
    spec.put_grant(granted("cache", dir_scope("cache", true)));
    spec.put_grant(granted("api", Scope::Net { at: text("example.com:443"), listen: false }));
    let manifest = manifest!(out: Out, cache: Dir<ReadWrite>, api: Net);
    let mut said = StdString::new();
    let status = launch_in(manifest, Some(&delivered(&spec)), io.clone(), &mut said, crate::launch_body!(writes; out: Out, cache: Dir<ReadWrite>, api: Net));
    assert_eq!((status, said.as_str(), written(&io).as_str()), (Status::OK, "", "wrote\n"));
}

#[test]
fn the_manifest_is_data_printed_when_asked() {
    let io = Io::sim(7);
    let (status, said, ran) = launch_cat(&io, Some(spec::MANIFEST_HEADER));
    assert_eq!((status, said.as_str(), ran), (Status::OK, "", false));
    assert_eq!(
        written(&io),
        "{\"params\":[{\"fills_itself\":true,\"kind\":\"out\",\"name\":\"out\"},\
         {\"fills_itself\":false,\"kind\":\"dir\",\"name\":\"data\",\"write\":false}],\"version\":1}\n"
    );
    assert_eq!(<Dir<ReadOnly> as Grant>::KIND, Kind::Dir { write: false });
}

/// D104: the owner's hello, `fn main(cx: &Cx, out: Out)`, says in its manifest that it takes
/// standard output and nothing else: one parameter `out` of kind `out`, which fills itself. A
/// `main` that still took a whole world could say nothing of what it reaches.
#[test]
fn the_hello_world_manifest_is_out_alone() {
    let manifest = manifest!(out: Out);
    assert_eq!(manifest.params, &[Param::new("out", Kind::Out)]);
    assert!(Kind::Out.fills_itself());
    let json = manifest.write(a());
    assert_eq!(json.as_str(), "{\"params\":[{\"fills_itself\":true,\"kind\":\"out\",\"name\":\"out\"}],\"version\":1}");
}

/// A value whose drop counts itself in the cell it borrows.
struct Counted<'c>(&'c Cell<u32>);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// A `main` that names the heap: a value boxed in it and a future pinned in it, both dropped,
/// each drop counted. 7 when both dropped once, and the future ran to its value.
fn boxes(_cx: &Cx, heap: crate::mem::Heap) -> Status {
    let dropped = Cell::new(0);
    let boxed = crate::boxed::Box::new_in(Counted(&dropped), heap);
    let counted = Counted(&dropped);
    let mut pinned = crate::boxed::Box::pin_in(
        async move {
            let _counted = counted;
            3u8
        },
        heap,
    );
    let mut context = core::task::Context::from_waker(core::task::Waker::noop());
    let ran = core::future::Future::poll(pinned.as_mut(), &mut context);
    drop((boxed, pinned));
    match (ran, dropped.get()) {
        (core::task::Poll::Ready(3), 2) => Status(7),
        _ => Status::FAILED,
    }
}

/// `main(cx, heap: Heap)` is handed the heap its entry took (D120): what it boxes and pins in
/// it is dropped, once each. The heap fills itself, so no spec grants it, and the manifest
/// says so. Prints `test tests::grants::a_main_that_names_the_heap_is_handed_it ... ok`.
#[test]
fn a_main_that_names_the_heap_is_handed_it() {
    let manifest = manifest!(heap: crate::mem::Heap);
    assert_eq!(manifest.params, &[Param::new("heap", Kind::Heap)]);
    assert_eq!(
        manifest.write(a()).as_str(),
        "{\"params\":[{\"fills_itself\":true,\"kind\":\"heap\",\"name\":\"heap\"}],\"version\":1}"
    );
    let mut said = StdString::new();
    let status = launch_in(manifest, None, Io::sim(9), &mut said, crate::launch_body!(boxes; heap: crate::mem::Heap));
    assert_eq!((status, said.as_str()), (Status(7), ""));
}

/// A `main` that does not name the heap is started as before: its manifest lists no heap,
/// and it runs with what it named. Prints
/// `test tests::grants::a_main_without_the_heap_is_handed_none ... ok`.
#[test]
fn a_main_without_the_heap_is_handed_none() {
    fn hello(cx: &Cx, out: Out) -> Result<(), Error> {
        out.line(cx, "no heap")?;
        Ok(())
    }
    let manifest = manifest!(out: Out);
    assert!(manifest.params.iter().all(|param| param.kind != Kind::Heap));
    let io = Io::sim(10);
    let mut said = StdString::new();
    let status = launch_in(manifest, None, io.clone(), &mut said, crate::launch_body!(hello; out: Out));
    assert_eq!((status, said.as_str(), written(&io).as_str()), (Status::OK, "", "no heap\n"));
}

/// The names table lists the heap: `heap`, of type `Heap`, filling itself, meeting no fault,
/// no scope to grant. Prints `test tests::grants::the_names_table_lists_the_heap ... ok`.
#[test]
fn the_names_table_lists_the_heap() {
    assert_eq!(Kind::Heap.name(), "heap");
    assert_eq!(<crate::mem::Heap as Grant>::KIND, Kind::Heap);
    assert!(Kind::Heap.fills_itself());
    assert!(Kind::fills_itself_named(b"heap"));
    assert!(Kind::FILLS_ITSELF.contains(&Kind::Heap));
    assert_eq!(Kind::Heap.scope_form(), "");
    assert_eq!(crate::test::reach(Kind::Heap), 0);
}

/// Each refusal: `main` never runs, nothing is written, the status is `REFUSED`, and the words
/// are that refusal's own, with its fix.
#[test]
fn each_refusal_says_its_own_fix_and_starts_nothing() {
    let refused = |fd3: Option<&[u8]>| {
        let io = with_fixture(8);
        let (status, said, ran) = launch_cat(&io, fd3);
        assert_eq!((status, ran, written(&io).as_str()), (Status::REFUSED, false, ""), "{said}");
        said
    };

    // Started with no spec, from a shell: the grant is missing and the start is named.
    let said = refused(None);
    assert!(said.contains("data (Dir<ReadOnly>): no grant; add `--grant data=<folder>`"), "{said}");
    assert!(said.contains("started with no run spec"), "{said}");

    // A spec that grants nothing.
    let said = refused(Some(&delivered(&bare())));
    assert!(said.starts_with("bstd: `main` was not started: 1 grant is missing or wrong\n"), "{said}");
    assert!(!said.contains("started with no run spec"), "{said}");

    // A grant of the wrong kind.
    let mut spec = bare();
    spec.put_grant(granted("data", Scope::Net { at: text("example.com:80"), listen: false }));
    let said = refused(Some(&delivered(&spec)));
    assert!(said.contains("data (Dir<ReadOnly>): granted a `net`, and it takes a `dir`; add `--grant data=<folder>`"), "{said}");

    // A grant for a parameter `main` lacks, beside a missing one: every problem at once.
    let mut spec = bare();
    spec.put_grant(granted("cache", dir_scope("cache", true)));
    let said = refused(Some(&delivered(&spec)));
    assert!(said.starts_with("bstd: `main` was not started: 2 grants are missing or wrong\n"), "{said}");
    assert!(said.contains("cache (dir): granted, and `main` has no parameter `cache`; remove its `--grant cache=..`"), "{said}");
    assert!(said.contains("data (Dir<ReadOnly>): no grant"), "{said}");

    // An unknown field.
    let mut good = bare();
    good.put_grant(granted("data", dir_scope("fixtures", false)));
    let json = StdString::from_utf8(good.write(a()).as_bytes().to_vec()).expect("UTF-8");
    let mut fd3 = spec::SPEC_HEADER.to_vec();
    fd3.extend_from_slice(json.replacen("\"version\":1", "\"colour\":\"red\",\"version\":1", 1).as_bytes());
    let said = refused(Some(&fd3));
    assert!(said.contains("unknown field \"colour\""), "{said}");

    // A grant's name twice.
    let one = "\"data\":{\"dir\":{\"path\":\"fixtures\",\"write\":false}}";
    let mut fd3 = spec::SPEC_HEADER.to_vec();
    fd3.extend_from_slice(json.replacen(one, &std::format!("{one},{one}"), 1).as_bytes());
    let said = refused(Some(&fd3));
    assert!(said.contains("`data` is given twice in one object"), "{said}");

    // No header: a descriptor 3 that was not the parent's.
    let said = refused(Some(b"{}"));
    assert!(said.contains("does not start with `bstd spec`"), "{said}");

    // The control: the same program with the good spec runs.
    let io = with_fixture(8);
    assert_eq!(launch_cat(&io, Some(&delivered(&good))).0, Status::OK);
}

#[test]
fn a_grant_never_widens_and_an_unbuilt_provider_is_refused() {
    let io = Io::sim(9);
    let manifest = manifest!(out: Out, cache: Dir<ReadWrite>, api: Net);
    let launch = |spec: &RunSpec<LaunchAlloc>| {
        let mut said = StdString::new();
        let body = crate::launch_body!(writes; out: Out, cache: Dir<ReadWrite>, api: Net);
        (launch_in(manifest, Some(&delivered(spec)), io.clone(), &mut said, body), said)
    };
    let mut spec = bare();
    spec.put_grant(granted("cache", dir_scope("cache", false)));
    spec.put_grant(Granted { provider: Some(Provider::Replay), ..granted("api", Scope::Net { at: text("example.com:443"), listen: false }) });
    let (status, said) = launch(&spec);
    assert_eq!(status, Status::REFUSED);
    assert!(said.contains("cache (Dir<ReadWrite>): granted read only, and it writes"), "{said}");
    assert!(said.contains("api (Net): its `replay provider` is not built yet"), "{said}");
    assert_eq!(written(&io), "");
}

/// The fake provider is its own world: a grant from it does not reach the real one's files.
#[test]
fn a_fake_grant_reaches_the_fake_world_only() {
    let io = with_fixture(10);
    let mut spec = bare();
    spec.put_grant(Granted { provider: Some(Provider::Fake), ..granted("data", dir_scope("fixtures", false)) });
    let (status, said, ran) = launch_cat(&io, Some(&delivered(&spec)));
    assert_eq!((status, ran), (Status::FAILED, true), "{said}");
    assert!(said.contains("hello.txt"), "the fake world has no fixture: {said}");
    // The control: the same grant from the real provider reads it.
    let mut spec = bare();
    spec.put_grant(granted("data", dir_scope("fixtures", false)));
    assert_eq!(launch_cat(&with_fixture(10), Some(&delivered(&spec))).0, Status::OK);
}

/// A spec drawn from a tape.
fn plan(tape: &Tape) -> RunSpec<LaunchAlloc> {
    const WORDS: [&str; 5] = ["a", "B_2", "fixtures", "x y \"q\" \\ é", ""];
    let word = |label| *tape.pick(&WORDS, label).expect("words");
    let mut args = Vec::new_in(a());
    for _ in 0..tape.below(4, "args") {
        args.push(text(word("arg")));
    }
    let mut env = Vec::new_in(a());
    for at in 0..tape.below(4, "vars") {
        env.push((text(["PATH", "HOME", "CC", "Z"][at as usize]), text(word("value"))));
    }
    let mut spec = RunSpec::defaults(args, env, text("/w"), a());
    const NAMES: [&str; 4] = ["data", "api", "vars", "io"];
    for _ in 0..tape.below(5, "grants") {
        let scope = match tape.below(6, "scope") {
            // A folder is a bstd path (relative, plain): the reader refuses any other.
            0 => dir_scope(tape.pick(&["fixtures", "a/b_2", "data.d"], "path").expect("paths"), tape.coin("write")),
            1 => Scope::Net { at: text("example.com:8080"), listen: tape.coin("listen") },
            2 => {
                let mut names = Vec::new_in(a());
                names.push(text("CC"));
                Scope::Env { names }
            }
            3 => Scope::Whole { capability: text("fs"), device: None },
            4 => Scope::Whole { capability: text("camera"), device: Some(text("front")) },
            _ => Scope::Whole { capability: text("rng"), device: None },
        };
        let mut one = granted(tape.pick(&NAMES, "name").expect("names"), scope);
        if tape.coin("provider") {
            one.provider = Some([Provider::Real, Provider::Fake, Provider::Replay][tape.index(3, "which")]);
        }
        if tape.coin("layers") {
            one.layers = Some(Layers::NONE.with(Layer::Record).with([Layer::Overlay, Layer::Ask][tape.index(2, "layer")]));
        }
        spec.put_grant(one);
    }
    spec.provider = [Provider::Real, Provider::Fake][tape.index(2, "run provider")];
    spec.seed = tape.next_u64("seed");
    spec.limits = RunLimits { memory: tape.coin("memory").then(|| tape.next_u64("most")), time_ms: tape.coin("time").then_some(1500) };
    spec.streams.stderr = match tape.coin("stderr") {
        true => Stream::File(text("err.log")),
        false => Stream::Null,
    };
    spec
}

/// The spec's round trip: what is read back is the spec, its text the same bytes, its digest the
/// same id; and one changed field changes the digest.
fn run(spec: &RunSpec<LaunchAlloc>, _plant: Plant) -> Result<(), StdString> {
    let bytes = spec.write(a());
    let back = RunSpec::read(bytes.as_bytes(), a()).map_err(|why| std::format!("{why}\n{}", bytes.as_str()))?;
    // A spec's value is its canonical text: equal bytes are the equal spec.
    if back.write(a()).as_bytes() != bytes.as_bytes() || back.digest(a()) != spec.digest(a()) {
        return Err(std::format!("the round trip changed the bytes of {}", bytes.as_str()));
    }
    let mut seeded = back;
    seeded.seed ^= 1;
    if seeded.digest(a()) == spec.digest(a()) {
        return Err("a changed seed kept the digest".into());
    }
    Ok(())
}

#[test]
fn a_spec_round_trips_to_equal_bytes_and_an_equal_digest() {
    blaze_sim::check(Config::cases(300), |tape| check(plan(tape), run));
}

/// `--grant` text, read by the parameter's kind.
#[test]
fn a_grant_argument_is_read_by_its_parameters_kind() {
    static M: Manifest =
        Manifest::new(&[Param::new("out", Kind::Out), Param::new("data", Kind::Dir { write: false }), Param::new("api", Kind::Net)]);
    let data = spec::grant_arg(&M, "data=fixtures", a()).expect("a folder");
    assert!(matches!(&data.scope, Scope::Dir { path, write: false } if path.as_str() == "fixtures"), "{:?}", data.scope);
    let api = spec::grant_arg(&M, "api=listen:localhost:8080", a()).expect("an endpoint");
    assert!(matches!(&api.scope, Scope::Net { at, listen: true } if at.as_str() == "localhost:8080"), "{:?}", api.scope);
    assert_eq!(spec::grant_arg(&M, "out=x", a()).map(|_| ()), Err(spec::GrantArgError::FillsItself));
    assert_eq!(spec::grant_arg(&M, "cache=x", a()).map(|_| ()), Err(spec::GrantArgError::NoParameter));
    assert_eq!(spec::grant_arg(&M, "data", a()).map(|_| ()), Err(spec::GrantArgError::NoEquals));
}
