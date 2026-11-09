//! The simulated providers, each once: a scripted value comes in through its capability and
//! what the capability did is recorded on the simulated host. These are not properties, only
//! that each capability's operations reach the host they are for; narrowing is
//! [`super::narrows`]' and absence [`super::missing`]'s.

use std::{collections::VecDeque, time::Duration};

use super::{path, root, world};
use crate::{
    Plant, Stopped,
    audio::{PERIOD, Pcm},
    blobs::{BlobId, Blobs, BlobsKind},
    device::{DeviceKind, Notice, Position, Precision, SimDevice},
    hosted::Refused,
    io::Io,
    keys::{SIGNATURE_LEN, SimKey},
    local::Local,
    name::{DeviceName, KeyName, ModuleName},
    net::NetKind,
    procs::{ProcId, Signal, SimProc},
    roots::Roots,
    streams::Streams,
    term::{Color, Key, KeyEvent, Size, Style, Styled},
    url::Url,
    window::{SimWindow, Surface, WindowEvent, WindowId},
};

fn sim() -> Io {
    world(7, Plant::None)
}

fn device(raw: &str) -> DeviceName {
    DeviceName::new(raw).expect("a test's device name")
}

#[test]
fn the_terminal_is_sized_written_and_typed_at_and_raw_mode_ends() {
    let io = sim();
    let cx = root(&io);
    let term = io.need_term().expect("the simulation has a terminal");
    io.script(|host| {
        host.term.size = Size { cols: 132, rows: 43 };
        host.term.keys.push_back(KeyEvent::plain(Key::Char('q')));
    });
    assert_eq!(term.size(&cx).expect("its size"), Size { cols: 132, rows: 43 });
    let bold = Style { color: Color::Red, bold: true };
    term.write(&cx, &[Styled { text: "stop", style: bold }]).expect("a write");
    assert_eq!(io.script(|host| host.term.written.clone()), Some(vec![("stop".to_owned(), bold)]));
    // The guard is linear: what is done under it is checked after it is ended.
    let raw = match term.raw(&cx) {
        Ok(raw) => raw,
        Err(error) => panic!("raw mode: {error}"),
    };
    let was_raw = io.script(|host| host.term.raw);
    let twice = match term.raw(&cx) {
        Ok(second) => {
            let _ = second.end(&cx);
            None
        }
        Err(error) => Some(error.kind),
    };
    let key = term.key(&cx);
    let ended = raw.end(&cx);
    assert_eq!(was_raw, Some(true));
    assert_eq!(twice, Some(Refused::Host("the terminal is in raw mode already")));
    assert_eq!(key.expect("the scripted key"), KeyEvent::plain(Key::Char('q')));
    ended.expect("raw mode ends");
    assert_eq!(io.script(|host| host.term.raw), Some(false));
    // No key is scripted now: the wait ends at its deadline, in a span that names it.
    let brief = cx.until(io.need_clock().expect("a clock").after(Duration::from_millis(5)));
    let stopped = term.key(&brief).expect_err("no key comes");
    assert!(matches!(stopped.kind, Refused::Stopped(Stopped::Deadline { span: "bstd::term key", .. })), "{stopped}");
}

#[test]
fn keys_sign_verify_decrypt_and_fetch_and_hardware_keys_stay_put() {
    let io = sim();
    let cx = root(&io);
    let (soft, hard) = (KeyName::new("deploy").expect("a name"), KeyName::new("enclave").expect("a name"));
    io.script(|host| {
        // clone: a test's names, refcounts.
        host.keys.push(SimKey { name: soft.clone(), material: vec![1, 2, 3], hardware: false });
        host.keys.push(SimKey { name: hard.clone(), material: vec![9], hardware: true });
    });
    let keys = io.need_keys().expect("the simulation has a keychain");
    let mut signature = cx.bytes();
    keys.sign(&cx, &soft, b"release 1", &mut signature).expect("a signature");
    assert_eq!(signature.len(), SIGNATURE_LEN);
    assert!(keys.verify(&cx, &soft, b"release 1", signature.as_slice()).expect("a verdict"));
    assert!(!keys.verify(&cx, &soft, b"release 2", signature.as_slice()).expect("a verdict"));
    assert!(!keys.verify(&cx, &hard, b"release 1", signature.as_slice()).expect("a verdict"));
    let sealed: Vec<u8> = b"open".iter().zip([1, 2, 3, 1]).map(|(byte, pad)| byte ^ pad).collect();
    let mut opened = cx.bytes();
    assert_eq!(keys.decrypt(&cx, &soft, &sealed, &mut opened).expect("a decryption"), 4);
    assert_eq!(opened.as_slice(), b"open");
    match keys.fetch(&cx, &soft) {
        Ok(secret) => assert_eq!(secret.expose(), [1, 2, 3]),
        Err(error) => panic!("a fetch: {error}"),
    }
    match keys.fetch(&cx, &hard) {
        Ok(_) => panic!("a key in hardware was fetched"),
        Err(error) => assert!(matches!(error.kind, Refused::Host(_)), "{error}"),
    }
    let missing = keys.verify(&cx, &KeyName::new("gone").expect("a name"), b"", b"").expect_err("no such key");
    assert_eq!(missing.kind, Refused::NotFound);
}

#[test]
fn roots_none_trusts_no_one_and_narrows_to_nothing() {
    let none = Roots::none();
    assert!(none.is_empty());
    let name = crate::name::Authority::new("anyone").expect("a name");
    assert!(!none.trusts(&name));
    assert!(none.only(std::slice::from_ref(&name)).is_empty());
}

#[test]
fn the_locale_and_the_user_are_the_hosts() {
    let io = sim();
    let cx = root(&io);
    let locale = io.need_locale().expect("the simulation has a locale");
    assert_eq!(locale, crate::locale::Locale::simulated());
    let user = io.need_user().expect("the simulation has a user");
    assert_eq!((user.name().as_str(), user.uid(), user.gid()), ("sim", 1000, 1000));
    // The home is a directory, with a directory's authority: a file written through it is
    // where the host says the home is.
    let home = user.home();
    home.write(&cx, &path("notes"), b"mine").expect("a write beneath the home");
    let fs = io.need_fs().expect("a file tree");
    let mut read = cx.bytes();
    fs.open_dir(&cx, &path("home/sim")).expect("the home").read_all(&cx, &path("notes"), &mut read).expect("a read");
    assert_eq!(read.as_slice(), b"mine");
}

#[test]
fn other_processes_are_listed_signalled_and_ended() {
    let io = sim();
    let cx = root(&io);
    io.script(|host| {
        host.procs.push(SimProc { id: ProcId(40), parent: ProcId(1), alive: true });
        host.procs.push(SimProc { id: ProcId(41), parent: ProcId(40), alive: true });
    });
    let procs = io.need_host().expect("a host").need_procs().expect("the simulation has processes");
    let listed = procs.list(&cx, cx.frame()).expect("a list");
    assert_eq!(listed.iter().map(|it| it.id).collect::<Vec<_>>(), [ProcId(40), ProcId(41)]);
    procs.signal(&cx, ProcId(41), Signal::Terminate).expect("a signal");
    assert!(!procs.inspect(&cx, ProcId(41)).expect("the process").alive);
    assert!(procs.inspect(&cx, ProcId(40)).expect("the process").alive);
    assert_eq!(io.script(|host| host.signals.clone()), Some(vec![(ProcId(41), Signal::Terminate)]));
    // Narrowed to one process, the list is that one and a signal to another is refused.
    let one = procs.only(ProcId(40));
    assert_eq!(one.list(&cx, cx.frame()).expect("a list").len(), 1);
    assert_eq!(one.signal(&cx, ProcId(41), Signal::Kill).expect_err("not its process").kind, Refused::NotAllowed);
    assert_eq!(procs.signal(&cx, ProcId(99), Signal::Kill).expect_err("no such process").kind, Refused::NotFound);
}

#[test]
fn code_loads_modules_and_a_narrowed_one_makes_nothing_executable() {
    let io = sim();
    let cx = root(&io);
    let (jit, other) = (ModuleName::new("jit").expect("a name"), ModuleName::new("other").expect("a name"));
    // clone: a test's name, a refcount.
    io.script(|host| host.modules.push(jit.clone()));
    let code = io.need_code().expect("the simulation has code");
    code.executable(&cx, &[0x90; 16]).expect("a region made executable");
    assert_eq!(code.load(&cx, &jit).expect("a load").name(), &jit);
    assert_eq!(code.load(&cx, &other).expect_err("no such module").kind, Refused::NotFound);
    let narrowed = code.only(&jit);
    assert_eq!(narrowed.executable(&cx, &[0x90]).expect_err("only a module").kind, Refused::NotAllowed);
    // clone: a test's name, a refcount.
    assert_eq!(io.script(|host| (host.executable.clone(), host.loaded.clone())), Some((vec![16], vec![jit.clone()])));
}

#[test]
fn a_window_has_a_surface_and_its_events_in_order() {
    let io = sim();
    let cx = root(&io);
    let surface = Surface { width: 800, height: 600 };
    let events = VecDeque::from([WindowEvent::Resized(surface), WindowEvent::Closed]);
    io.script(|host| host.windows.push(SimWindow { id: WindowId(3), surface, events, pixels: Vec::new() }));
    let window = io.need_windows().expect("the simulation has windows").only(WindowId(3));
    assert_eq!(window.surface(&cx, WindowId(3)).expect("its surface"), surface);
    assert_eq!(window.event(&cx, WindowId(3)).expect("an event"), WindowEvent::Resized(surface));
    assert_eq!(window.event(&cx, WindowId(3)).expect("an event"), WindowEvent::Closed);
    assert_eq!(window.event(&cx, WindowId(4)).expect_err("not its window").kind, Refused::NotAllowed);
}

#[test]
fn the_opener_and_automation_record_what_they_were_asked() {
    let io = sim();
    let cx = root(&io);
    let app = crate::name::AppName::new("editor").expect("a name");
    // clone: a test's name, a refcount.
    io.script(|host| host.apps.push(app.clone()));
    let url = Url::parse("https://example.com/docs").expect("a URL");
    io.need_opener().expect("the simulation has an opener").open(&cx, &url).expect("an open");
    // clone: a test's URL, refcounts.
    assert_eq!(io.script(|host| host.opened.clone()), Some(vec![url.clone()]));
    let key = KeyEvent { key: Key::Char('s'), ctrl: true, alt: false };
    io.need_automation().expect("the simulation has automation").press(&cx, &app, key).expect("a key pressed");
    assert_eq!(io.script(|host| host.pressed.clone()), Some(vec![(app, key)]));
}

#[test]
fn power_reads_the_battery_and_a_hold_is_let_go() {
    let io = sim();
    let cx = root(&io);
    let power = io.need_power().expect("the simulation has power");
    io.script(|host| host.battery = crate::power::Battery { percent: 12, charging: false });
    assert_eq!(power.battery(&cx).expect("the battery").percent, 12);
    // The hold is linear: ended, and only then is the machine free to sleep.
    let awake = match power.keep_awake(&cx) {
        Ok(awake) => awake,
        Err(error) => panic!("a hold: {error}"),
    };
    let held = io.script(|host| host.awake);
    let ended = awake.end(&cx);
    assert_eq!(held, Some(1));
    ended.expect("the hold ends");
    assert_eq!(io.script(|host| host.awake), Some(0));
}

#[test]
fn played_samples_are_the_models_and_recorded_ones_the_script_then_silence() {
    let io = sim();
    let cx = root(&io);
    let (speaker, microphone) = (device("speaker"), device("microphone"));
    let heard: Vec<i16> = (0..PERIOD as i16 + 10).collect();
    io.script(|host| {
        // clone: a test's names, refcounts.
        host.audio.outputs.push(speaker.clone());
        host.audio.inputs.push(microphone.clone());
        host.audio.heard = heard.iter().copied().collect();
    });
    let audio = io.need_audio().expect("the simulation has sound");
    let pcm = Pcm { rate: 48_000, channels: 1 };
    // A ramp: sample `n` of the stream is `n`, whatever the period it falls in.
    let mut next: i16 = 0;
    let mut ramp = |period: &mut [i16]| {
        for sample in period {
            *sample = next;
            next = next.wrapping_add(1);
        }
    };
    assert_eq!(audio.out().play(&cx, &speaker, pcm, 3, &mut ramp).expect("a stream played"), 3 * PERIOD);
    let model: Vec<i16> = (0..3 * PERIOD as i16).collect();
    assert_eq!(io.script(|host| host.audio.played.clone()), Some(model));
    let mut recorded: Vec<i16> = Vec::with_capacity(2 * PERIOD);
    let mut take = |period: &[i16]| recorded.extend_from_slice(period);
    let input = audio.need_input().expect("the simulation records");
    assert_eq!(input.record(&cx, &microphone, pcm, 2, &mut take).expect("a stream recorded"), 2 * PERIOD);
    let mut model = heard;
    model.resize(2 * PERIOD, 0);
    assert_eq!(recorded, model);
    let bad = audio.out().play(&cx, &speaker, Pcm { rate: 0, channels: 1 }, 1, &mut |_: &mut [i16]| {});
    assert!(matches!(bad.expect_err("no such format").kind, Refused::Host(_)));
    let unknown = audio.out().play(&cx, &device("nowhere"), pcm, 1, &mut |_: &mut [i16]| {});
    assert_eq!(unknown.expect_err("no such device").kind, Refused::NotFound);
}

#[test]
fn each_device_reads_its_script_and_records_what_it_was_sent() {
    let io = sim();
    let cx = root(&io);
    let name = device("unit");
    io.script(|host| {
        for (kind, input) in [
            (DeviceKind::Camera, b"frame".to_vec()),
            (DeviceKind::Display, b"pixels".to_vec()),
            (DeviceKind::Usb, b"usb in".to_vec()),
            (DeviceKind::Serial, b"serial in".to_vec()),
            (DeviceKind::Bluetooth, Vec::new()),
        ] {
            // clone: a test's name, a refcount.
            host.devices.push(SimDevice { kind, name: name.clone(), input, output: Vec::new() });
        }
        host.position = Position { lat_e7: 485_512_345, lon_e7: 23_498_765 };
        host.clipboard = b"copied".to_vec();
    });
    let output = |kind: DeviceKind| {
        io.script(|host| host.devices.iter().find(|it| it.kind == kind).map(|it| it.output.clone())).flatten()
    };
    let mut read = cx.bytes();
    io.need_camera().expect("a camera").frame(&cx, &name, &mut read).expect("a frame");
    // clone: a test's name, a refcount.
    let display = crate::device::Target::Display(name.clone());
    io.need_screen().expect("a screen").capture(&cx, &display, &mut read).expect("a capture");
    let usb = io.need_usb().expect("usb");
    usb.read(&cx, &name, &mut read).expect("a read");
    usb.write(&cx, &name, b"usb out").expect("a write");
    let serial = io.need_serial().expect("serial");
    serial.read(&cx, &name, &mut read).expect("a read");
    serial.write(&cx, &name, b"serial out").expect("a write");
    assert_eq!(read.as_slice(), b"framepixelsusb inserial in");
    assert_eq!(output(DeviceKind::Usb).as_deref(), Some(&b"usb out"[..]));
    assert_eq!(output(DeviceKind::Serial).as_deref(), Some(&b"serial out"[..]));

    let bluetooth = io.need_bluetooth().expect("bluetooth");
    assert_eq!(bluetooth.scan(&cx, cx.frame()).expect("a scan").as_slice(), std::slice::from_ref(&name));
    let service = crate::name::ServiceName::new("heart-rate").expect("a name");
    bluetooth.write(&cx, &name, &service, b"\x01").expect("a write");
    assert_eq!(io.script(|host| host.bluetooth_sent.len()), Some(1));

    let location = io.need_location().expect("a location");
    assert_eq!(location.fix(&cx).expect("a fix"), Position { lat_e7: 485_512_345, lon_e7: 23_498_765 });
    let city = location.coarser(Precision::City);
    assert_eq!(city.fix(&cx).expect("a fix"), Position { lat_e7: 485_500_000, lon_e7: 23_400_000 });
    assert_eq!(city.coarser(Precision::Exact).precision(), Precision::City);

    let clipboard = io.need_clipboard().expect("a clipboard");
    let mut copied = cx.bytes();
    clipboard.read(&cx, &mut copied).expect("a read");
    assert_eq!(copied.as_slice(), b"copied");
    clipboard.write(&cx, b"new").expect("a write");
    assert_eq!(io.script(|host| host.clipboard.clone()), Some(b"new".to_vec()));

    io.need_notify().expect("notifications").post(&cx, &Notice { title: "done", body: "the build passed" }).expect("a post");
    assert_eq!(io.script(|host| host.notices.clone()), Some(vec![("done".to_owned(), "the build passed".to_owned())]));
}

#[test]
fn a_resolver_looks_names_up_and_cannot_connect() {
    let io = sim();
    let cx = root(&io);
    io.script(|host| host.names.push(("api.example".to_owned(), std::net::IpAddr::from([10, 1, 2, 3]))));
    let resolver = io.need_net().expect("a net").resolver();
    let name = crate::net::HostPort::parse("api.example:443").expect("a host and port");
    assert_eq!(resolver.lookup(&cx, &name).expect("an address").to_string(), "10.1.2.3:443");
    let literal = crate::net::HostPort::parse("[::1]:80").expect("a host and port");
    assert_eq!(resolver.lookup(&cx, &literal).expect("an address").to_string(), "[::1]:80");
    let unknown = crate::net::HostPort::parse("nowhere.example:1").expect("a host and port");
    assert_eq!(resolver.lookup(&cx, &unknown).expect_err("no such name").kind, NetKind::NoSuchName);
    // A resolver from a Net that connects to one host resolves only it.
    let narrowed = io.need_net().expect("a net").connect_only(&crate::net::Addr::Tcp(literal)).resolver();
    assert_eq!(narrowed.lookup(&cx, &name).expect_err("not its host").kind, NetKind::NotAllowed);
}

#[test]
fn blobs_are_kept_under_what_they_are_and_checked_as_they_are_read() {
    let io = sim();
    let cx = root(&io);
    let dir = io.need_fs().expect("a file tree").open_dir(&cx, &path("objects")).expect("a directory");
    let blobs = Blobs::beneath(&dir);
    let id = blobs.put(&cx, b"some bytes").expect("a put");
    assert_eq!(id, BlobId::of(b"some bytes"));
    let mut read = cx.bytes();
    assert_eq!(blobs.get(&cx, &id, &mut read).expect("a get"), Some(10));
    assert_eq!(read.as_slice(), b"some bytes");
    assert!(blobs.has(&cx, &id).expect("a has"));
    assert_eq!(blobs.list(&cx, cx.frame()).expect("a list").as_slice(), [id]);
    assert_eq!(blobs.get(&cx, &BlobId::of(b"other"), &mut read).expect("a get"), None);
    // Bytes under an id that are not its are refused, and nothing of them is handed back.
    dir.write(&cx, &path(&id.to_string()), b"tampered").expect("a write behind the store's back");
    let corrupt = blobs.read_only().get(&cx, &id, &mut read).expect_err("the bytes are not the blob");
    assert_eq!(corrupt.kind, BlobsKind::Corrupt { found: BlobId::of(b"tampered") });
    assert_eq!(read.as_slice(), b"some bytes");
    blobs.delete(&cx, &id).expect("a delete");
    assert!(!blobs.has(&cx, &id).expect("a has"));
}

#[test]
fn local_sockets_beneath_a_directory_connect_and_a_read_only_one_does_not_listen() {
    let io = sim();
    let cx = root(&io);
    let dir = io.need_fs().expect("a file tree").open_dir(&cx, &path("run")).expect("a directory");
    let local = Local::beneath(&dir);
    let reader = Local::beneath(&dir.read_only());
    let refused = match reader.listen(&cx, &path("daemon.sock")) {
        Ok(listener) => {
            let _ = listener.close(&cx);
            None
        }
        Err(error) => Some(error.kind),
    };
    assert_eq!(refused, Some(NetKind::NotAllowed));
    // Each of the three is linear: every one made is finished, and what they did is checked
    // after.
    let mut listener = match local.listen(&cx, &path("daemon.sock")) {
        Ok(listener) => listener,
        Err(error) => panic!("a listen: {error}"),
    };
    let mut got = cx.bytes();
    let said = match reader.connect(&cx, &path("daemon.sock")) {
        Ok(client) => {
            let said = match listener.accept(&cx) {
                Ok(server) => {
                    let sent = client.send(&cx, b"hello");
                    let received = server.recv(&cx, 16, &mut got);
                    let finished = server.finish(&cx);
                    sent.is_ok() && received.is_ok() && finished.is_ok()
                }
                Err(_) => false,
            };
            client.finish(&cx).is_ok() && said
        }
        Err(_) => false,
    };
    let closed = listener.close(&cx);
    assert!(said, "the connection through the socket");
    assert_eq!(got.as_slice(), b"hello");
    closed.expect("the listener closes");
}

#[test]
fn streams_from_a_handler_reply_and_from_the_host_record() {
    let io = sim();
    let cx = root(&io);
    let url = Url::parse("https://relay.example/session").expect("a URL");
    let origin = url.origin().expect("an origin");
    let shout = Streams::handler(|_: &crate::Cx, _: &crate::url::Origin, sent: &[u8], reply: &mut crate::mem::Bytes<'_>| {
        reply.try_extend(&sent.to_ascii_uppercase())
    });
    let mut heard = cx.bytes();
    // A stream is linear: finished whatever it carried, and checked after.
    let carried = match shout.open(&cx, origin) {
        Ok(mut stream) => {
            let sent = stream.send(&cx, b"quiet");
            let first = stream.recv(&cx, 3, &mut heard);
            let rest = stream.recv(&cx, 64, &mut heard);
            let end = stream.recv(&cx, 64, &mut heard);
            let finished = stream.finish(&cx);
            sent.is_ok() && first == Ok(3) && rest == Ok(2) && end == Ok(0) && finished.is_ok()
        }
        Err(_) => false,
    };
    assert!(carried, "a stream to a handler");
    assert_eq!(heard.as_slice(), b"QUIET");

    io.script(|host| host.streams.replies.push_back(b"welcome".to_vec()));
    let hosted = io.need_streams().expect("the simulation carries streams");
    let mut greeted = cx.bytes();
    let carried = match hosted.open(&cx, origin) {
        Ok(mut stream) => {
            let sent = stream.send(&cx, b"hi");
            let received = stream.recv(&cx, 64, &mut greeted);
            let finished = stream.finish(&cx);
            sent.is_ok() && received == Ok(7) && finished.is_ok()
        }
        Err(_) => false,
    };
    assert!(carried, "a stream through the host");
    assert_eq!(greeted.as_slice(), b"welcome");
    // clone: a test's origin, refcounts.
    let sent = io.script(|host| (host.streams.sent.clone(), host.streams.finished));
    assert_eq!(sent, Some((vec![(origin.clone(), b"hi".to_vec())], 1)));
}
