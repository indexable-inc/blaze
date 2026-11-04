//! Code written through one mapping of an entry runs through an overlay of another, rights the
//! kernel caps, this process's own image, the rendezvous between two ends of a socket in this
//! process, and its messages decoded from any bytes without a panic.

use blaze_ids::rng::SplitMix64;

use super::rendezvous::{self, Message, Request, Token, decode_reply, decode_request, encode_reply};
use super::{Granted, Object, View, granted, overlay, own_image, page, thread_id, threads};
use crate::channel::Channel;
use crate::frame::kinds;
use crate::handshake::{Overlay, Place};
use crate::sys::Rights;
use crate::{Access, Error};

/// `mov w0, #42` and `ret`.
const FORTY_TWO: [u8; 8] = [0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6];

fn pair() -> (Channel, Channel) {
    let mut fds = [0; 2];
    // SAFETY: `fds` is two writable ints for the new sockets.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) }, 0);
    (Channel::from_raw(i64::from(fds[0])).unwrap(), Channel::from_raw(i64::from(fds[1])).unwrap())
}

fn over(at: u64, len: u64, access: Access, place: Place) -> Overlay {
    Overlay { object: 0, offset: 0, len, at, access, place, image: false }
}

/// Pages of this process for an overlay to replace, as dyld's mapping of an executable is.
fn place(len: u64) -> View {
    Object::create("place", len).unwrap().map_here(Access::Read).unwrap()
}

#[test]
fn code_overlaid_from_one_view_runs_what_the_other_writes() {
    let (mut channel, _other) = pair();
    let object = Object::create("test", page()).unwrap();
    let mut view = object.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..8).unwrap().copy_from_slice(&FORTY_TWO);
    view.publish(0..8);

    let code: Granted = granted(&object, Rights::Code).unwrap();
    let place = place(page());
    let at = place.address();
    overlay(&mut channel, std::slice::from_ref(&code), &[over(at, page(), Access::Execute, Place::Code)], 0).unwrap();
    // SAFETY: `at` maps the object's first page readable and executable, which holds
    // `mov w0, #42; ret` made right for instruction fetch above.
    let run: extern "C" fn() -> u32 = unsafe { std::mem::transmute(at as usize) };
    assert_eq!(run(), 42);

    view.bytes_mut(0..4).unwrap().copy_from_slice(&[0xe0, 0x00, 0x80, 0x52]);
    view.publish(0..4);
    assert_eq!(run(), 7, "a store through the daemon's view is what the other view runs");

    let error = overlay(&mut channel, std::slice::from_ref(&code), &[over(at, page(), Access::Shared, Place::Reserved)], 0)
        .unwrap_err();
    assert!(error.what.contains("mach_vm_map"), "the code entry carries no write right: {error}");
}

#[test]
fn a_copy_overlay_is_private_and_code_maps_anywhere_only_to_read() {
    let (mut channel, _other) = pair();
    let len = page();
    let object = Object::create("copy", len).unwrap();
    let mut view = object.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..4).unwrap().copy_from_slice(&[1, 2, 3, 4]);

    let code = granted(&object, Rights::Code).unwrap();
    let place = place(len);
    let at = place.address();
    overlay(&mut channel, std::slice::from_ref(&code), &[over(at, len, Access::Copy, Place::Reserved)], 0).unwrap();
    // SAFETY: `at` is the private copy just mapped, readable and writable, of `len` bytes.
    let copy = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, len as usize) };
    assert_eq!(&copy[..4], &[1, 2, 3, 4]);
    copy[0] = 9;
    assert_eq!(view.bytes(0..1).unwrap(), &[1]);

    let read = code.map_anywhere(&mut channel, Access::Read).unwrap();
    // SAFETY: `read` is the whole object mapped readable just now, for the rest of the test.
    assert_eq!(unsafe { std::slice::from_raw_parts(read as *const u8, 4) }, &[1, 2, 3, 4]);
    assert!(code.map_anywhere(&mut channel, Access::Shared).is_err(), "the code entry carries no write right");
    assert!(code.map_anywhere(&mut channel, Access::Execute).is_err(), "code runs only where an overlay puts it");
    let past = Overlay { offset: len, ..over(at, len, Access::Read, Place::Reserved) };
    assert!(overlay(&mut channel, std::slice::from_ref(&code), &[past], 0).is_err(), "past the object");
}

#[test]
fn the_image_of_this_process_is_its_executable() {
    let (header, headers) = own_image().unwrap();
    let layout = crate::image::layout(crate::Format::MachO, headers, page()).unwrap();
    let slide = header.wrapping_sub(layout.header);
    let here = (own_image as usize as u64).wrapping_sub(slide);
    assert!(
        layout.code.iter().any(|it| (it.linked..it.linked + it.len).contains(&here)),
        "{here:#x} is in no code segment of {:?}",
        layout.code
    );
}

#[test]
fn threads_name_this_one() {
    let mut out = Vec::new();
    threads(std::process::id(), &mut out).unwrap();
    assert!(out.contains(&thread_id()), "{out:?} lacks {}", thread_id());
    let worker = std::thread::spawn(thread_id);
    let id = worker.join().unwrap();
    assert_ne!(id, thread_id());
}

#[test]
fn the_rendezvous_hands_objects_to_the_peer() {
    let (mut daemon, mut hook) = pair();
    let image = Object::create("image", page()).unwrap();
    let control = Object::create("control", page()).unwrap();
    let mut view = image.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..3).unwrap().copy_from_slice(b"abc");

    let taker = std::thread::spawn(move || -> Result<Vec<Granted>, Error> {
        let frame = hook.recv()?;
        assert_eq!(frame.kind, kinds::GRANT);
        assert_eq!(frame.body, b"grant");
        let granted = super::recv_objects(&mut hook, 2)?;
        let read = granted[0].map_anywhere(&mut hook, Access::Read)?;
        // SAFETY: the image object mapped readable just now, for the rest of the process.
        assert_eq!(unsafe { std::slice::from_raw_parts(read as *const u8, 3) }, b"abc");
        assert!(granted[0].map_anywhere(&mut hook, Access::Shared).is_err());
        let words = granted[1].map_anywhere(&mut hook, Access::Shared)?;
        // SAFETY: the control object mapped shared and writable just now, page-aligned.
        unsafe { std::ptr::write_volatile(words as *mut u64, 7) };
        Ok(granted)
    });
    super::send_objects(&mut daemon, std::process::id(), b"grant", &[(&image, Rights::Code), (&control, Rights::Words)])
        .unwrap();
    let granted = taker.join().unwrap().unwrap();
    assert_eq!(granted.iter().map(Granted::len).collect::<Vec<_>>(), [page(), page()]);
    assert_eq!(control.map_here(Access::Shared).unwrap().word(0).load(std::sync::atomic::Ordering::Acquire), 7);
}

#[test]
fn a_grant_for_another_process_is_refused() {
    let (mut daemon, _hook) = pair();
    let image = Object::create("other", page()).unwrap();
    let error = super::send_objects(&mut daemon, std::process::id() + 1, b"", &[(&image, Rights::Code)]).unwrap_err();
    assert!(error.what.contains("not the run process"), "{error}");
}

fn token(seed: u32) -> Token {
    std::array::from_fn(|index| seed.wrapping_mul(31).wrapping_add(index as u32))
}

fn with_trailer(message: &mut Message, size: usize, token: &Token) {
    let at = size.next_multiple_of(4);
    message.0[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    message.0[at + 4..at + 8].copy_from_slice(&52u32.to_le_bytes());
    for (index, word) in token.iter().enumerate() {
        message.0[at + 20 + index * 4..at + 24 + index * 4].copy_from_slice(&word.to_le_bytes());
    }
}

/// A request as the kernel delivers it: the reply port in the remote field, a send-once right.
fn received_request(reply: u32, count: u32, token: &Token) -> Message {
    let mut message = Message::new();
    rendezvous::encode_request(&mut message, 0x1234, reply, count);
    message.0[0..4].copy_from_slice(&18u32.to_le_bytes());
    message.0[8..12].copy_from_slice(&reply.to_le_bytes());
    with_trailer(&mut message, 32, token);
    message
}

#[test]
fn messages_round_trip() {
    let message = received_request(0x707, 3, &token(5));
    assert_eq!(decode_request(&message.0), Some(Request { reply: 0x707, count: 3, token: token(5) }));

    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    for round in 0..200 {
        let count = rng.index(33);
        let objects: Vec<(u32, u64)> = (0..count).map(|_| (rng.next_u32() | 1, rng.next_u64())).collect();
        let objects: Vec<(u32, u64)> = objects.into_iter().map(|(port, len)| (port.min(u32::MAX - 1), len)).collect();
        let mut reply = Message::new();
        let size = encode_reply(&mut reply, 0x55, &objects) as usize;
        with_trailer(&mut reply, size, &token(round));
        assert_eq!(decode_reply(&reply.0, count), Ok((objects.clone(), token(round))));
        if count > 0 {
            assert!(decode_reply(&reply.0, count - 1).is_err());
        }
    }
}

#[test]
fn messages_from_any_bytes_decode_or_fail() {
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    let request = received_request(9, 2, &token(1));
    let mut reply = Message::new();
    let size = encode_reply(&mut reply, 9, &[(3, 4), (5, 6)]) as usize;
    with_trailer(&mut reply, size, &token(2));
    for _ in 0..20_000 {
        let mut bytes = if rng.coin() { request.0 } else { reply.0 };
        for _ in 0..1 + rng.below(8) {
            let at = rng.index(128);
            bytes[at] = rng.next_u64() as u8;
        }
        let len = rng.index(bytes.len());
        let _ = decode_request(&bytes[..len]);
        let _ = decode_reply(&bytes[..len], rng.index(40));
        let _ = decode_request(&bytes);
        let _ = decode_reply(&bytes, 2);
    }
}
