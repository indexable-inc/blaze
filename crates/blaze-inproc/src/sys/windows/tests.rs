use std::time::Duration;

use super::*;

fn granule() -> u64 {
    page()
}

#[test]
fn the_granule_is_64_kib() {
    assert_eq!(page(), 64 << 10);
}

#[test]
fn two_views_of_one_object_see_one_memory() {
    let object = Object::create("test", 2 * granule()).unwrap();
    let mut writer = object.map_here(Access::Shared).unwrap();
    let reader = object.duplicate().unwrap().map_here(Access::Read).unwrap();
    assert_ne!(writer.address(), reader.address());
    assert!(reader.bytes(0..16).unwrap().iter().all(|it| *it == 0));
    writer.bytes_mut(granule() as usize - 4..granule() as usize + 4).unwrap().copy_from_slice(b"blazing!");
    assert_eq!(reader.bytes(granule() as usize - 4..granule() as usize + 4).unwrap(), b"blazing!");
    writer.word(8).store(0x1234_5678_9abc_def0, std::sync::atomic::Ordering::Release);
    assert_eq!(reader.word(8).load(std::sync::atomic::Ordering::Acquire), 0x1234_5678_9abc_def0);
    writer.publish(0..64);
}

#[test]
fn a_read_view_refuses_writes_and_out_of_bounds() {
    let object = Object::create("test", granule()).unwrap();
    let mut view = object.map_here(Access::Read).unwrap();
    assert!(view.bytes_mut(0..1).is_err());
    assert!(view.bytes(0..granule() as usize + 1).is_err());
    #[allow(clippy::reversed_empty_ranges)]
    let backwards = 8..4;
    assert!(view.bytes(backwards).is_err());
}

#[test]
fn a_copy_is_private() {
    let object = Object::create("test", granule()).unwrap();
    let mut shared = object.map_here(Access::Shared).unwrap();
    let mut copy = object.map_here(Access::Copy).unwrap();
    shared.bytes_mut(0..4).unwrap().copy_from_slice(b"daem");
    assert_eq!(copy.bytes(0..4).unwrap(), b"daem");
    copy.bytes_mut(0..4).unwrap().copy_from_slice(b"mine");
    assert_eq!(shared.bytes(0..4).unwrap(), b"daem");
    assert_eq!(copy.bytes(0..4).unwrap(), b"mine");
}

#[test]
fn an_image_span_splits_at_its_overlays() {
    let g = 0x1_0000;
    let span = 0x10 * g..0x20 * g;
    assert_eq!(pieces(span.clone(), &[]).unwrap(), [Piece::Private { at: 0x10 * g, len: 0x10 * g }]);
    assert_eq!(pieces(span.clone(), &[0x11 * g..0x13 * g, 0x13 * g..0x14 * g, 0x18 * g..0x20 * g]).unwrap(), [
        Piece::Private { at: 0x10 * g, len: g },
        Piece::View { at: 0x11 * g, len: 2 * g, index: 0 },
        Piece::View { at: 0x13 * g, len: g, index: 1 },
        Piece::Private { at: 0x14 * g, len: 4 * g },
        Piece::View { at: 0x18 * g, len: 8 * g, index: 2 },
    ]);
    assert!(pieces(span.clone(), &[0x12 * g..0x13 * g, 0x11 * g..0x12 * g]).is_err(), "out of order");
    assert!(pieces(span.clone(), &[0x1f * g..0x21 * g]).is_err(), "past the span");
    assert!(pieces(span, &[0x12 * g..0x12 * g]).is_err(), "empty");
}

#[test]
fn what_an_image_keeps_is_outside_its_overlays() {
    let overlays = [0x2000..0x4000, 0x6000..0x7000];
    assert_eq!(outside(0x1000..0x8000, &overlays), [0x1000..0x2000, 0x4000..0x6000, 0x7000..0x8000]);
    assert_eq!(outside(0x2000..0x4000, &overlays), Vec::<Range<u64>>::new());
    assert_eq!(outside(0x3000..0x5000, &overlays), [0x4000..0x5000]);
    assert_eq!(outside(0x8000..0x9000, &overlays), [0x8000..0x9000]);
}

#[test]
fn kept_pages_are_data_only() {
    assert_eq!(private_protection(PAGE_WRITECOPY).unwrap(), PAGE_READWRITE);
    assert_eq!(private_protection(PAGE_READONLY).unwrap(), PAGE_READONLY);
    assert!(private_protection(PAGE_EXECUTE_READ).is_err(), "code outside an overlay would run from the file");
    assert!(private_protection(PAGE_READONLY | 0x100).is_err(), "a guard page");
}

#[test]
fn views_go_on_granules_only() {
    assert!(granular(0, granule(), 0).is_ok());
    assert!(granular(0, 0, 0).is_err());
    assert!(granular(4096, granule(), 0).is_err());
    assert!(granular(0, 4096, 0).is_err());
    assert!(granular(0, granule(), 4096).is_err());
}

#[test]
fn rights_bound_what_a_run_process_maps() {
    assert!(allowed(Rights::Code, Access::Execute));
    assert!(allowed(Rights::Code, Access::Read));
    assert!(allowed(Rights::Code, Access::Copy));
    assert!(!allowed(Rights::Code, Access::Shared));
    assert!(allowed(Rights::Words, Access::Shared));
    assert!(allowed(Rights::Words, Access::Read));
    assert!(!allowed(Rights::Words, Access::Execute));
    assert!(!allowed(Rights::Words, Access::Copy));
}

/// A connected pair: the daemon's end (with the peer's ID) and the start hook's.
fn pair() -> ((Channel, u32), Channel) {
    let dir = std::env::temp_dir();
    let (listener, address) = Listener::bind(&dir).unwrap();
    let accepting = std::thread::spawn(move || listener.accept().unwrap());
    let loader = Channel::new(connect(&address).unwrap());
    let (stream, pid) = accepting.join().unwrap();
    ((Channel::new(stream), pid), loader)
}

#[test]
fn the_pipe_carries_frames_and_names_the_peer() {
    let ((mut daemon, pid), mut loader) = pair();
    assert_eq!(pid, std::process::id());
    loader.send(kinds::HELLO, 7, b"hello").unwrap();
    let frame = daemon.recv().unwrap();
    assert_eq!((frame.kind, frame.seq, frame.body), (kinds::HELLO, 7, &b"hello"[..]));
    let long = vec![0x5a; 300 << 10];
    daemon.send(kinds::GRANT, 0, &long).unwrap();
    assert_eq!(loader.recv().unwrap().body, &long[..]);
}

#[test]
fn a_read_times_out_and_the_pipe_still_works() {
    let ((mut daemon, _), mut loader) = pair();
    daemon.set_timeout(Some(Duration::from_millis(50))).unwrap();
    let error = daemon.recv().unwrap_err();
    assert!(error.what.contains("nothing within"), "{error}");
    loader.send(kinds::HELLO, 1, b"late").unwrap();
    assert_eq!(daemon.recv().unwrap().body, b"late");
}

#[test]
fn a_hang_up_is_said() {
    let ((mut daemon, _), loader) = pair();
    drop(loader);
    let error = daemon.recv().unwrap_err();
    assert!(error.what.contains("hung up"), "{error}");
}

#[test]
fn only_pipes_of_blaze_inproc_are_opened() {
    assert!(connect(r"C:\Windows\notepad.exe").is_err());
    assert!(connect(r"\\.\pipe\someone-else").is_err());
}

/// Serves MAP frames as the host does, until the start hook hangs up.
fn serve(mut channel: Channel, remote: Remote) {
    loop {
        let Ok(frame) = channel.recv() else { return };
        let (kind, seq, body) = (frame.kind, frame.seq, frame.body.to_vec());
        assert_eq!(kind, kinds::MAP);
        let (object, access) = crate::handshake::decode_map(&body).unwrap();
        let mut answer = Vec::new();
        match remote.map(object, access) {
            Ok(address) => {
                Writer::new(&mut answer).u64(address);
                channel.send(kinds::MAPPED, seq, &answer).unwrap();
            }
            Err(error) => {
                Writer::new(&mut answer).str(&error.to_string());
                channel.send(kinds::REFUSED, seq, &answer).unwrap();
            }
        }
    }
}

#[test]
fn the_daemon_maps_granted_objects_into_the_hook() {
    let ((mut daemon, pid), mut hook) = pair();
    let image = Object::create("image", 2 * granule()).unwrap();
    let control = Object::create("control", granule()).unwrap();
    let mut view = image.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..4).unwrap().copy_from_slice(b"code");
    let remote = send_objects(&mut daemon, pid, b"grant", &[(&image, Rights::Code), (&control, Rights::Words)]).unwrap();
    let serving = std::thread::spawn(move || serve(daemon, remote));

    let frame = hook.recv().unwrap();
    assert_eq!((frame.kind, frame.seq, frame.body), (kinds::GRANT, 0, &b"grant"[..]));
    let objects = recv_objects(&mut hook, 2).unwrap();
    assert_eq!((objects[0].len(), objects[1].len()), (2 * granule(), granule()));

    let read = objects[0].map_anywhere(&mut hook, Access::Read).unwrap();
    // SAFETY: the daemon just mapped the image object readable here, for the rest of the test.
    assert_eq!(unsafe { *(read as *const [u8; 4]) }, *b"code");
    view.bytes_mut(4..8).unwrap().copy_from_slice(b"more");
    // SAFETY: as above.
    assert_eq!(unsafe { *((read + 4) as *const [u8; 4]) }, *b"more", "the daemon's writes show through");

    assert!(objects[0].map_anywhere(&mut hook, Access::Shared).is_err(), "code shared writable");
    assert!(objects[0].map_anywhere(&mut hook, Access::Execute).is_err(), "code anywhere but an overlay");
    assert!(objects[1].map_anywhere(&mut hook, Access::Execute).is_err(), "executable words");
    let words = objects[1].map_anywhere(&mut hook, Access::Shared).unwrap();
    // SAFETY: the control object, mapped shared and writable here just now.
    unsafe { std::ptr::write_volatile(words as *mut u64, 7) };
    assert_eq!(control.map_here(Access::Read).unwrap().word(0).load(std::sync::atomic::Ordering::Acquire), 7);
    drop(hook);
    serving.join().unwrap();
}

#[test]
fn this_process_names_its_own_image() {
    let (base, headers) = own_image().unwrap();
    let layout = crate::image::layout(crate::Format::Pe, headers, page());
    assert!(base.is_multiple_of(granule()));
    // A test binary is linked by the pinned toolchain, whose sections sit on 4 KiB: refused,
    // naming the granule, never read as a layout to overlay.
    if let Err(error) = layout {
        assert!(error.what.contains("page"), "{error}");
    }
}

#[test]
fn a_listed_thread_is_one_of_ours() {
    let mut threads = Vec::new();
    super::threads(std::process::id(), &mut threads).unwrap();
    assert!(threads.contains(&thread_id()));
    let other = std::thread::spawn(thread_id).join().unwrap();
    assert_ne!(other, thread_id());
    super::threads(u32::MAX - 3, &mut threads).unwrap();
    assert!(threads.is_empty());
}

#[test]
fn a_backtrace_reaches_the_outermost_frame() {
    let mut frames = [0u64; 512];
    let len = backtrace(&mut frames).unwrap();
    assert!(len > 2, "{len} frames");
    assert!(frames[..len].iter().all(|it| *it != 0));
    let mut few = [0u64; 1];
    assert_eq!(backtrace(&mut few), None);
}

#[test]
fn random_bytes_differ() {
    let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
    random(&mut a).unwrap();
    random(&mut b).unwrap();
    assert_ne!(a, b);
}

#[test]
fn serializing_returns() {
    serialize();
}
