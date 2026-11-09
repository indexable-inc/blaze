//! Narrowing never widens, for every capability of "The rest of the set" that has a narrowing:
//! one harness over [`Narrows`], a trait of "narrow, then try each thing". After any chain of
//! narrowings a capability reaches exactly the things every narrowing in the chain kept: never
//! one more (it widened) and never one fewer (it lost what it held).
//!
//! Each case scripts a simulated host in which every thing is there, so a thing a capability
//! does not reach is refused by the capability, not missing from the host.
//!
//! Plant: [`Plant::OnlyWidens`], a narrowing that gives what was asked for, reached before or
//! not. One control a case: each must catch it.

use std::collections::VecDeque;

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root, world};
use crate::{
    Cx, Plant,
    audio::{AudioIn, AudioOut, Pcm},
    automation::Automation,
    code::Code,
    device::{
        Bluetooth, Camera, Clipboard, DeviceKind, Location, Position, Precision, Screen, Serial, SimDevice, Target,
        Usb,
    },
    hosted::Refused,
    io::Io,
    keys::{Keys, SimKey},
    local::Local,
    name::{AppName, Authority, DeviceName, KeyName, ModuleName, ServiceName},
    net::{HostPort, NetKind},
    opener::Opener,
    procs::{ProcId, Procs, SimProc},
    requests::{Http, HttpKind, Request, SimRoute},
    resolver::Resolver,
    roots::{Anchor, Roots},
    streams::Streams,
    term::{Key, KeyEvent},
    url::{Origin, Url},
    window::{SimWindow, Surface, Window, WindowId},
};

/// A capability with a narrowing, as the harness tries it.
trait Narrows {
    type Cap;
    /// How many things the whole capability reaches.
    const THINGS: usize;
    /// How many narrowings there are to pick from.
    const STEPS: usize;
    /// The whole capability, in a world scripted so that every thing is there.
    fn whole(io: &Io, cx: &Cx) -> Result<Self::Cap, String>;
    /// `cap` narrowed by the narrowing `step`.
    fn narrow(cap: &Self::Cap, step: usize) -> Self::Cap;
    /// The things the narrowing `step` keeps, one bit a thing: the model.
    fn keeps(step: usize) -> u64;
    /// Whether `cap` reaches `thing`: `false` only when the capability refused it as not
    /// allowed; any other failure is the harness's.
    fn reaches(cap: &Self::Cap, io: &Io, cx: &Cx, thing: usize) -> Result<bool, String>;
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// Each narrowing, taken modulo the case's `STEPS`.
    chain: Vec<u64>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    // 840 is divisible by every `STEPS` here, so each narrowing is as likely as another.
    let chain = (0..d.int(1, 5)).map(|_| d.int(0, 839)).collect();
    Plan { seed, chain }
}

fn run<N: Narrows>(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let mut cap = N::whole(&io, &cx)?;
    let mut allowed: u64 = (1 << N::THINGS) - 1;
    for thing in 0..N::THINGS {
        if !N::reaches(&cap, &io, &cx, thing)? {
            return Err(format!("harness: the whole capability does not reach thing {thing}"));
        }
    }
    for (at, &step) in plan.chain.iter().enumerate() {
        let step = (step % N::STEPS as u64) as usize;
        cap = N::narrow(&cap, step);
        allowed &= N::keeps(step);
        for thing in 0..N::THINGS {
            let reaches = N::reaches(&cap, &io, &cx, thing)?;
            let may = allowed & (1 << thing) != 0;
            match (reaches, may) {
                (true, false) => {
                    return Err(format!(
                        "after narrowing {at} (step {step}) the capability reaches thing {thing}, which the chain does not allow: it widened"
                    ));
                }
                (false, true) => {
                    return Err(format!(
                        "after narrowing {at} (step {step}) the capability lost thing {thing}, which every narrowing kept"
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Whether a call on a hosted capability reached its thing.
fn reached<T, E: std::fmt::Display>(got: Result<T, E>, kind: impl Fn(&E) -> &Refused) -> Result<bool, String> {
    match got {
        Ok(_) => Ok(true),
        Err(error) if *kind(&error) == Refused::NotAllowed => Ok(false),
        Err(error) => Err(format!("harness: {error}")),
    }
}

/// Three names of a kind.
fn three<T>(make: impl Fn(&str) -> T) -> [T; 3] {
    [make("first"), make("second"), make("third")]
}

fn device(raw: &str) -> DeviceName {
    DeviceName::new(raw).expect("a test's device name")
}

fn scripted(io: &Io, f: impl FnOnce(&mut crate::hosted::SimHost)) -> Result<(), String> {
    io.script(f).ok_or_else(|| "harness: the world is not simulated".to_owned())
}

/// The devices `first`, `second` and `third` of `kind`, each with something to read.
fn devices(io: &Io, kind: DeviceKind) -> Result<(), String> {
    scripted(io, |host| {
        for name in three(device) {
            host.devices.push(SimDevice { kind, name, input: vec![1, 2, 3], output: Vec::new() });
        }
    })
}

/// The subset of `all` the mask `step` spells.
fn subset<T: Clone>(all: &[T; 3], step: usize) -> Vec<T> {
    (0..3).filter(|at| step & (1 << at) != 0).map(|at| all[at].clone()).collect()
}

struct KeysCase;

impl Narrows for KeysCase {
    type Cap = Keys;
    const THINGS: usize = 3;
    const STEPS: usize = 8;

    fn whole(io: &Io, _: &Cx) -> Result<Keys, String> {
        scripted(io, |host| {
            for name in three(|raw| KeyName::new(raw).expect("a test's key name")) {
                host.keys.push(SimKey { name, material: vec![7; 4], hardware: false });
            }
        })?;
        io.need_keys().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Keys, step: usize) -> Keys {
        cap.only(&subset(&three(|raw| KeyName::new(raw).expect("a test's key name")), step))
    }

    fn keeps(step: usize) -> u64 {
        step as u64
    }

    fn reaches(cap: &Keys, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        let key = &three(|raw| KeyName::new(raw).expect("a test's key name"))[thing];
        reached(cap.fetch(cx, key), |error| &error.kind)
    }
}

struct RootsCase;

impl Narrows for RootsCase {
    type Cap = Roots;
    const THINGS: usize = 3;
    const STEPS: usize = 8;

    fn whole(io: &Io, _: &Cx) -> Result<Roots, String> {
        scripted(io, |host| {
            for name in three(|raw| Authority::new(raw).expect("a test's authority")) {
                host.roots.push(Anchor { name, der: std::sync::Arc::from(vec![0x30]) });
            }
        })?;
        io.need_roots().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Roots, step: usize) -> Roots {
        cap.only(&subset(&three(|raw| Authority::new(raw).expect("a test's authority")), step))
    }

    fn keeps(step: usize) -> u64 {
        step as u64
    }

    fn reaches(cap: &Roots, _: &Io, _: &Cx, thing: usize) -> Result<bool, String> {
        Ok(cap.trusts(&three(|raw| Authority::new(raw).expect("a test's authority"))[thing]))
    }
}

struct ProcsCase;

impl Narrows for ProcsCase {
    type Cap = Procs;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Procs, String> {
        scripted(io, |host| {
            for id in 0..3 {
                host.procs.push(SimProc { id: ProcId(10 + id), parent: ProcId(1), alive: true });
            }
        })?;
        io.need_host().and_then(|host| host.need_procs()).map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Procs, step: usize) -> Procs {
        cap.only(ProcId(10 + step as u32))
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Procs, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.inspect(cx, ProcId(10 + thing as u32)), |error| &error.kind)
    }
}

fn module(raw: &str) -> ModuleName {
    ModuleName::new(raw).expect("a test's module name")
}

struct CodeCase;

impl Narrows for CodeCase {
    type Cap = Code;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Code, String> {
        scripted(io, |host| host.modules.extend(three(module)))?;
        io.need_code().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Code, step: usize) -> Code {
        cap.only(&three(module)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Code, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.load(cx, &three(module)[thing]), |error| &error.kind)
    }
}

struct WindowCase;

impl Narrows for WindowCase {
    type Cap = Window;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Window, String> {
        scripted(io, |host| {
            for id in 0..3 {
                let surface = Surface { width: 640, height: 480 };
                host.windows.push(SimWindow { id: WindowId(id), surface, events: VecDeque::new(), pixels: vec![9] });
            }
        })?;
        io.need_windows().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Window, step: usize) -> Window {
        cap.only(WindowId(step as u32))
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Window, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.surface(cx, WindowId(thing as u32)), |error| &error.kind)
    }
}

fn app(raw: &str) -> AppName {
    AppName::new(raw).expect("a test's application name")
}

struct AutomationCase;

impl Narrows for AutomationCase {
    type Cap = Automation;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Automation, String> {
        scripted(io, |host| host.apps.extend(three(app)))?;
        io.need_automation().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Automation, step: usize) -> Automation {
        cap.only(&three(app)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Automation, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.press(cx, &three(app)[thing], KeyEvent::plain(Key::Enter)), |error| &error.kind)
    }
}

/// A URL of each of three schemes.
fn links() -> [Url; 3] {
    ["https://example.com/a", "mailto:someone@example.com", "file:///tmp/x"]
        .map(|raw| Url::parse(raw).expect("a test's URL"))
}

struct OpenerCase;

impl Narrows for OpenerCase {
    type Cap = Opener;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Opener, String> {
        io.need_opener().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Opener, step: usize) -> Opener {
        cap.only(links()[step].scheme())
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Opener, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.open(cx, &links()[thing]), |error| &error.kind)
    }
}

const PCM: Pcm = Pcm { rate: 48_000, channels: 2 };

struct AudioOutCase;

impl Narrows for AudioOutCase {
    type Cap = AudioOut;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<AudioOut, String> {
        scripted(io, |host| host.audio.outputs.extend(three(device)))?;
        Ok(io.need_audio().map_err(|e| format!("harness: {e}"))?.out())
    }

    fn narrow(cap: &AudioOut, step: usize) -> AudioOut {
        cap.only(&three(device)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &AudioOut, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.play(cx, &three(device)[thing], PCM, 1, &mut |_: &mut [i16]| {}), |error| &error.kind)
    }
}

struct AudioInCase;

impl Narrows for AudioInCase {
    type Cap = AudioIn;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<AudioIn, String> {
        scripted(io, |host| host.audio.inputs.extend(three(device)))?;
        io.need_audio().and_then(|audio| audio.need_input()).map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &AudioIn, step: usize) -> AudioIn {
        cap.only(&three(device)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &AudioIn, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.record(cx, &three(device)[thing], PCM, 1, &mut |_: &[i16]| {}), |error| &error.kind)
    }
}

struct CameraCase;

impl Narrows for CameraCase {
    type Cap = Camera;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Camera, String> {
        devices(io, DeviceKind::Camera)?;
        io.need_camera().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Camera, step: usize) -> Camera {
        cap.only(&three(device)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Camera, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.frame(cx, &three(device)[thing], &mut cx.bytes()), |error| &error.kind)
    }
}

/// Two displays and a window.
fn targets() -> [Target; 3] {
    [Target::Display(device("first")), Target::Display(device("second")), Target::Window(WindowId(7))]
}

struct ScreenCase;

impl Narrows for ScreenCase {
    type Cap = Screen;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Screen, String> {
        devices(io, DeviceKind::Display)?;
        scripted(io, |host| {
            let surface = Surface { width: 8, height: 8 };
            host.windows.push(SimWindow { id: WindowId(7), surface, events: VecDeque::new(), pixels: vec![5; 4] });
        })?;
        io.need_screen().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Screen, step: usize) -> Screen {
        cap.only(&targets()[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Screen, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.capture(cx, &targets()[thing], &mut cx.bytes()), |error| &error.kind)
    }
}

/// Where the simulated machine is: a place whose rounding to a city and to a country differ
/// from it and from each other.
const HERE: Position = Position { lat_e7: 371_234_567, lon_e7: -1_221_234_567 };

const PRECISIONS: [Precision; 3] = [Precision::Exact, Precision::City, Precision::Country];

struct LocationCase;

impl Narrows for LocationCase {
    type Cap = Location;
    /// Thing 0: the exact place is learned. Thing 1: the city is.
    const THINGS: usize = 2;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Location, String> {
        scripted(io, |host| host.position = HERE)?;
        io.need_location().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Location, step: usize) -> Location {
        cap.coarser(PRECISIONS[step])
    }

    fn keeps(step: usize) -> u64 {
        [0b11, 0b10, 0b00][step]
    }

    fn reaches(cap: &Location, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        let fix = cap.fix(cx).map_err(|e| format!("harness: {e}"))?;
        Ok(match thing {
            0 => fix == HERE,
            _ => Precision::City.round(fix) == Precision::City.round(HERE),
        })
    }
}

fn service(raw: &str) -> ServiceName {
    ServiceName::new(raw).expect("a test's service name")
}

struct BluetoothCase;

impl Narrows for BluetoothCase {
    type Cap = Bluetooth;
    /// Thing `2 * device + service`, of two devices and two services.
    const THINGS: usize = 4;
    /// Only the first device, only the second, only the first service, only the second.
    const STEPS: usize = 4;

    fn whole(io: &Io, _: &Cx) -> Result<Bluetooth, String> {
        devices(io, DeviceKind::Bluetooth)?;
        io.need_bluetooth().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Bluetooth, step: usize) -> Bluetooth {
        match step {
            0 | 1 => cap.only(&three(device)[step]),
            _ => cap.only_service(&three(service)[step - 2]),
        }
    }

    fn keeps(step: usize) -> u64 {
        [0b0011, 0b1100, 0b0101, 0b1010][step]
    }

    fn reaches(cap: &Bluetooth, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        let (at, of) = (&three(device)[thing / 2], &three(service)[thing % 2]);
        reached(cap.write(cx, at, of, b"x"), |error| &error.kind)
    }
}

struct UsbCase;

impl Narrows for UsbCase {
    type Cap = Usb;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Usb, String> {
        devices(io, DeviceKind::Usb)?;
        io.need_usb().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Usb, step: usize) -> Usb {
        cap.only(&three(device)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Usb, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.write(cx, &three(device)[thing], b"x"), |error| &error.kind)
    }
}

struct SerialCase;

impl Narrows for SerialCase {
    type Cap = Serial;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Serial, String> {
        devices(io, DeviceKind::Serial)?;
        io.need_serial().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Serial, step: usize) -> Serial {
        cap.only(&three(device)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Serial, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        reached(cap.read(cx, &three(device)[thing], &mut cx.bytes()), |error| &error.kind)
    }
}

struct ClipboardCase;

impl Narrows for ClipboardCase {
    type Cap = Clipboard;
    /// Thing 0: reading. Thing 1: writing.
    const THINGS: usize = 2;
    /// Read only, write only.
    const STEPS: usize = 2;

    fn whole(io: &Io, _: &Cx) -> Result<Clipboard, String> {
        io.need_clipboard().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Clipboard, step: usize) -> Clipboard {
        match step {
            0 => cap.read_only(),
            _ => cap.write_only(),
        }
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Clipboard, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        match thing {
            0 => reached(cap.read(cx, &mut cx.bytes()), |error| &error.kind),
            _ => reached(cap.write(cx, b"copied"), |error| &error.kind),
        }
    }
}

/// Three origins, and a URL of each.
fn origins() -> [Url; 3] {
    ["https://first.example/a", "https://second.example/a", "http://third.example:8080/a"]
        .map(|raw| Url::parse(raw).expect("a test's URL"))
}

fn origin(at: usize) -> Origin {
    // clone: a test's origin, refcounts.
    origins()[at].origin().expect("a test's URL has an origin").clone()
}

struct HttpCase;

impl Narrows for HttpCase {
    type Cap = Http;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Http, String> {
        scripted(io, |host| {
            for url in origins() {
                host.http.routes.push(SimRoute { url, status: 200, body: b"ok".to_vec() });
            }
        })?;
        io.need_http().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Http, step: usize) -> Http {
        cap.only(&origin(step))
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Http, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        match cap.request(cx, &Request::get(&origins()[thing]), &mut cx.bytes()) {
            Ok(_) => Ok(true),
            Err(error) if error.kind == HttpKind::NotAllowed => Ok(false),
            Err(error) => Err(format!("harness: {error}")),
        }
    }
}

struct StreamsCase;

impl Narrows for StreamsCase {
    type Cap = Streams;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Streams, String> {
        io.need_streams().map_err(|e| format!("harness: {e}"))
    }

    fn narrow(cap: &Streams, step: usize) -> Streams {
        cap.only(&origin(step))
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Streams, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        // A stream is linear: one that opened is finished here.
        match cap.open(cx, &origin(thing)) {
            Ok(stream) => match stream.finish(cx) {
                Ok(()) => Ok(true),
                Err(error) => Err(format!("harness: {error}")),
            },
            Err(error) if error.kind == Refused::NotAllowed => Ok(false),
            Err(error) => Err(format!("harness: {error}")),
        }
    }
}

/// Three hosts by name, each in the simulation's name service.
fn hosts() -> [HostPort; 3] {
    ["first.example:443", "second.example:443", "third.example:8080"]
        .map(|raw| HostPort::parse(raw).expect("a test's host and port"))
}

struct ResolverCase;

impl Narrows for ResolverCase {
    type Cap = Resolver;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, _: &Cx) -> Result<Resolver, String> {
        scripted(io, |host| {
            for (at, name) in hosts().iter().enumerate() {
                let ip = std::net::IpAddr::from([10, 0, 0, at as u8 + 1]);
                host.names.push((name.host().to_owned(), ip));
            }
        })?;
        Ok(io.need_net().map_err(|e| format!("harness: {e}"))?.resolver())
    }

    fn narrow(cap: &Resolver, step: usize) -> Resolver {
        cap.only(&hosts()[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Resolver, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        match cap.lookup(cx, &hosts()[thing]) {
            Ok(_) => Ok(true),
            Err(error) if error.kind == NetKind::NotAllowed => Ok(false),
            Err(error) => Err(format!("harness: {error}")),
        }
    }
}

struct LocalCase;

impl Narrows for LocalCase {
    type Cap = Local;
    const THINGS: usize = 3;
    const STEPS: usize = 3;

    fn whole(io: &Io, cx: &Cx) -> Result<Local, String> {
        let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
        let dir = fs.open_dir(cx, &path("run")).map_err(|e| format!("harness: {e}"))?;
        Ok(Local::beneath(&dir))
    }

    fn narrow(cap: &Local, step: usize) -> Local {
        cap.only(&three(path)[step])
    }

    fn keeps(step: usize) -> u64 {
        1 << step
    }

    fn reaches(cap: &Local, _: &Io, cx: &Cx, thing: usize) -> Result<bool, String> {
        // A listener is linear: one that was made is closed here.
        match cap.listen(cx, &three(path)[thing]) {
            Ok(listener) => match listener.close(cx) {
                Ok(()) => Ok(true),
                Err(error) => Err(format!("harness: {error}")),
            },
            Err(error) if error.kind == NetKind::NotAllowed => Ok(false),
            Err(error) => Err(format!("harness: {error}")),
        }
    }
}

/// The property and its control, for one case.
macro_rules! narrowing {
    ($case:ident, $never_widens:ident, $caught:ident) => {
        #[hegel::test(test_cases = 40)]
        fn $never_widens(tc: TestCase) {
            check(plan(&mut Drawn(&tc)), run::<$case>);
        }

        #[test]
        fn $caught() {
            control(Plant::OnlyWidens, |seeds: &mut Seeds| plan(seeds), run::<$case>);
        }
    };
}

narrowing!(KeysCase, keys_only_never_widens, the_oracle_catches_keys_that_widen);
narrowing!(RootsCase, roots_only_never_widens, the_oracle_catches_roots_that_widen);
narrowing!(ProcsCase, procs_only_never_widens, the_oracle_catches_procs_that_widen);
narrowing!(CodeCase, code_only_never_widens, the_oracle_catches_code_that_widens);
narrowing!(WindowCase, window_only_never_widens, the_oracle_catches_a_window_that_widens);
narrowing!(AutomationCase, automation_only_never_widens, the_oracle_catches_automation_that_widens);
narrowing!(OpenerCase, opener_only_never_widens, the_oracle_catches_an_opener_that_widens);
narrowing!(AudioOutCase, audio_out_only_never_widens, the_oracle_catches_audio_out_that_widens);
narrowing!(AudioInCase, audio_in_only_never_widens, the_oracle_catches_audio_in_that_widens);
narrowing!(CameraCase, camera_only_never_widens, the_oracle_catches_a_camera_that_widens);
narrowing!(ScreenCase, screen_only_never_widens, the_oracle_catches_a_screen_that_widens);
narrowing!(LocationCase, location_coarser_never_refines, the_oracle_catches_a_location_that_refines);
narrowing!(BluetoothCase, bluetooth_only_never_widens, the_oracle_catches_bluetooth_that_widens);
narrowing!(UsbCase, usb_only_never_widens, the_oracle_catches_usb_that_widens);
narrowing!(SerialCase, serial_only_never_widens, the_oracle_catches_serial_that_widens);
narrowing!(ClipboardCase, clipboard_narrowing_never_widens, the_oracle_catches_a_clipboard_that_widens);
narrowing!(HttpCase, http_only_never_widens, the_oracle_catches_http_that_widens);
narrowing!(StreamsCase, streams_only_never_widens, the_oracle_catches_streams_that_widen);
narrowing!(ResolverCase, resolver_only_never_widens, the_oracle_catches_a_resolver_that_widens);
narrowing!(LocalCase, local_only_never_widens, the_oracle_catches_local_that_widens);
