//! Device grants (one_device.rs) through the launcher's test entry ([`launch_in`]) on the
//! simulation, and their spellings in the spec and in `--grant`. `main`'s status says what its
//! call met (`Status(n)`, n chosen per case), so nothing is kept aside.

use crate::{
    audio::{AudioIn, AudioOut, Pcm},
    cx::Cx,
    device::{Camera, Notify},
    entry::{Status, launch_in},
    gpu::Gpu,
    grant::{LaunchAlloc, Manifest, Param},
    hosted::Refused,
    io::{Io, Offers},
    name::DeviceName,
    spec::{GrantArgError, GrantName, Granted, RunSpec, Scope, delivery, grant_arg},
};

fn alloc() -> LaunchAlloc {
    blaze_own::testing::heap().shared()
}

fn text(raw: &str, alloc: LaunchAlloc) -> blaze_own::String<LaunchAlloc> {
    blaze_own::String::from_str_in(raw, alloc)
}

/// The grant of `capability` whole, or of its one `device`.
fn whole(capability: &str, device: Option<&str>, a: LaunchAlloc) -> Scope<LaunchAlloc> {
    Scope::Whole { capability: text(capability, a), device: device.map(|it| text(it, a)) }
}

fn spec(grants: &[(&str, Scope<LaunchAlloc>)], a: LaunchAlloc) -> RunSpec<LaunchAlloc> {
    let mut spec = RunSpec::defaults(blaze_own::Vec::new_in(a), blaze_own::Vec::new_in(a), text("/work", a), a);
    for (name, scope) in grants {
        let name = GrantName::new(name, a).expect("a grant's name");
        spec.put_grant(Granted { name, scope: scope.clone(), provider: None, layers: None });
    }
    spec
}

/// `body` launched as `manifest`'s program on `io` under `grants`: its status and words.
fn launch<R: crate::entry::Outcome>(
    manifest: &'static Manifest,
    io: Io,
    grants: &[(&str, Scope<LaunchAlloc>)],
    body: impl FnOnce(&mut crate::grant::Given<'_>) -> Result<R, crate::grant::Refused>,
) -> (Status, std::string::String) {
    let a = alloc();
    let fd3 = delivery(&spec(grants, a), a);
    let mut said = std::string::String::new();
    let status = launch_in(manifest, Some(&fd3), io, &mut said, body);
    (status, said)
}

/// What a sound call met: 1 the narrowing refused it, 2 it passed the narrowing and the
/// simulated host has no such device (it has none), 3 anything else.
fn met<T>(result: Result<T, crate::audio::AudioError>) -> Status {
    match result {
        Err(error) if error.kind == Refused::NotAllowed => Status(1),
        Err(error) if error.kind == Refused::NotFound => Status(2),
        _ => Status(3),
    }
}

const PCM: Pcm = Pcm { rate: 48_000, channels: 2 };

static SPEAKER: Manifest = Manifest::new(&[Param::of::<AudioOut>("speaker")]);

fn plays_on_right(cx: &Cx, speaker: AudioOut) -> Status {
    let right = DeviceName::new("right").expect("a device name");
    met(speaker.play(cx, &right, PCM, 1, &mut |_period: &mut [i16]| {}))
}

/// A device grant reaches that device only: `speaker` granted the device `left` is refused
/// `right` by its narrowing (1). Control: granted whole, the same call passes the narrowing and
/// meets the simulated host, which has no `right` (2).
/// Expected: `test grant::one_device::tests::a_device_grant_reaches_that_device_only ... ok`.
#[test]
fn a_device_grant_reaches_that_device_only() {
    let a = alloc();
    let body = || crate::launch_body!(plays_on_right; speaker: AudioOut);
    let (status, said) = launch(&SPEAKER, Io::sim(1), &[("speaker", whole("audio", Some("left"), a))], body());
    assert_eq!((status, said.as_str()), (Status(1), ""));
    let (status, said) = launch(&SPEAKER, Io::sim(1), &[("speaker", whole("audio", None, a))], body());
    assert_eq!((status, said.as_str()), (Status(2), ""));
}

static MIC: Manifest = Manifest::new(&[Param::of::<AudioIn>("mic")]);

fn records_from_mic(cx: &Cx, mic: AudioIn) -> Status {
    let device = DeviceName::new("mic").expect("a device name");
    met(mic.record(cx, &device, PCM, 1, &mut |_period: &[i16]| {}))
}

/// Recording is its own kind: `mic` granted `audio` (playing) is refused as the wrong
/// capability; granted `audio_in` it starts and records (the host has no such device: 2).
/// Expected: `test grant::one_device::tests::recording_is_its_own_kind ... ok`.
#[test]
fn recording_is_its_own_kind() {
    let a = alloc();
    let body = || crate::launch_body!(records_from_mic; mic: AudioIn);
    let (status, said) = launch(&MIC, Io::sim(1), &[("mic", whole("audio", None, a))], body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("- mic (audio_in)"), "{said}");
    let (status, said) = launch(&MIC, Io::sim(1), &[("mic", whole("audio_in", None, a))], body());
    assert_eq!((status, said.as_str()), (Status(2), ""));
}

static CAMERA: Manifest = Manifest::new(&[Param::of::<Camera>("camera")]);

fn takes_camera(_cx: &Cx, camera: Camera) -> Status {
    let _ = camera;
    Status(9)
}

static CAMERA_ORDINARY: Manifest = Manifest::new(&[Param::of::<Camera>("camera")]);

/// An ordinary start (no spec on file descriptor 3) with no grant gives the process's camera
/// whole, as a `whole!` capability's would. Control: the same start on a world that offers no
/// camera is refused by name. Its device grant narrows on such a start too, which is
/// a_device_grant_reaches_that_device_only's check (the launcher reads the grant either way).
/// Expected: `test grant::one_device::tests::an_ordinary_start_takes_the_process_s_device ... ok`.
#[test]
fn an_ordinary_start_takes_the_process_s_device() {
    let body = || crate::launch_body!(takes_camera; camera: Camera);
    let mut said = std::string::String::new();
    let status = launch_in(&CAMERA_ORDINARY, None, Io::sim(1), &mut said, body());
    assert_eq!((status, said.as_str()), (Status(9), ""));
    let mut said = std::string::String::new();
    let status = launch_in(&CAMERA_ORDINARY, None, Io::sim_with(1, Offers::NATIVE), &mut said, body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("this host offers no `camera`"), "{said}");
}

/// A real machine offers no device yet, so a program that asks for one is refused before
/// `main`, by the capability's name. Control: the simulation, which offers it, starts it.
/// Expected: `test grant::one_device::tests::a_machine_without_the_device_refuses_it_by_name ... ok`.
#[test]
fn a_machine_without_the_device_refuses_it_by_name() {
    let a = alloc();
    let body = || crate::launch_body!(takes_camera; camera: Camera);
    let grants = [("camera", whole("camera", Some("front"), a))];
    let (status, said) = launch(&CAMERA, Io::sim_with(1, Offers::NATIVE), &grants, body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("this host offers no `camera`"), "{said}");
    let (status, said) = launch(&CAMERA, Io::sim(1), &grants, body());
    assert_eq!((status, said.as_str()), (Status(9), ""));
}

static GPU: Manifest = Manifest::new(&[Param::of::<Gpu>("gpu")]);

fn takes_gpu(_cx: &Cx, gpu: Gpu) -> Status {
    let _ = gpu;
    Status(10)
}

/// A `Gpu` is granted whole (`whole!`), and the simulation has no device, so a granted `gpu`
/// is refused by name there; with no grant the refusal says so instead (the control: the first
/// refusal is the device's, not the grant's).
/// Expected: `test grant::one_device::tests::a_gpu_is_granted_whole_and_the_simulation_has_none ... ok`.
#[test]
fn a_gpu_is_granted_whole_and_the_simulation_has_none() {
    let a = alloc();
    let body = || crate::launch_body!(takes_gpu; gpu: Gpu);
    let (status, said) = launch(&GPU, Io::sim(1), &[("gpu", whole("gpu", None, a))], body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("this host offers no `gpu`"), "{said}");
    let (status, said) = launch(&GPU, Io::sim(1), &[], body());
    assert_eq!(status, Status::REFUSED, "{said}");
    assert!(said.contains("no grant"), "{said}");
    assert_eq!(GPU.write(alloc()).as_str(), r#"{"params":[{"capability":"gpu","fills_itself":false,"kind":"whole","name":"gpu"}],"version":1}"#);
}

static NOTIFY: Manifest = Manifest::new(&[Param::of::<Notify>("notify"), Param::of::<AudioOut>("speaker")]);

/// The spellings: `--grant speaker=device:left` gives the device grant, and the spec carries it
/// as `{"capability", "device"}` and reads it back equal; a whole grant keeps its old text. A
/// device for a capability that is not narrowed by device (`notify`) is refused by both.
/// Expected: `test grant::one_device::tests::a_device_grant_is_spelled_and_read_back ... ok`.
#[test]
fn a_device_grant_is_spelled_and_read_back() {
    let a = alloc();
    let granted = grant_arg(&NOTIFY, "speaker=device:left", a).expect("a device grant");
    // `Scope<A>`'s derived `PartialEq` needs `A: PartialEq`, which a launch allocator is not.
    let is_whole = |s: &Scope<LaunchAlloc>, cap: &str, dev: Option<&str>| {
        matches!(s, Scope::Whole { capability, device } if capability.as_str() == cap && device.as_ref().map(|d| d.as_str()) == dev)
    };
    assert!(is_whole(&granted.scope, "audio", Some("left")), "{:?}", granted.scope);
    let plain = grant_arg(&NOTIFY, "speaker=whole", a).expect("a whole grant").scope;
    assert!(is_whole(&plain, "audio", None), "{plain:?}");
    assert!(matches!(grant_arg(&NOTIFY, "notify=device:left", a), Err(GrantArgError::Value { .. })));
    assert!(matches!(grant_arg(&NOTIFY, "speaker=device: left", a), Err(GrantArgError::Value { .. })));

    let one = spec(&[("speaker", whole("audio", Some("left"), a)), ("notify", whole("notify", None, a))], a);
    let written = one.write(a);
    assert!(written.as_str().contains(r#""whole":{"capability":"audio","device":"left"}"#), "{}", written.as_str());
    assert!(written.as_str().contains(r#""whole":"notify""#), "{}", written.as_str());
    let back = RunSpec::read(written.as_bytes(), a).expect("the spec reads back");
    assert_eq!(back.write(a).as_str(), written.as_str(), "the spec reads back equal");

    let wrong = written.as_str().replace(r#""whole":"notify""#, r#""whole":{"capability":"notify","device":"left"}"#);
    assert!(RunSpec::read(wrong.as_bytes(), a).is_err(), "a device of `notify` is refused");
}
