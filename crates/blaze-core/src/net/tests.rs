use std::{
    os::{
        fd::AsRawFd as _,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
};

use super::{Socket, listen};
use crate::{Address, FileName};

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bz-inherited-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn inherited(fd: i32) -> Address {
    Address::Inherited { fd, at: FileName::new("blaze.sock").unwrap() }
}

fn refused(root: &std::path::Path, at: &Address) -> String {
    match listen(root, at) {
        Ok(_) => panic!("{at:?} was listened on"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn an_inherited_socket_bound_at_its_name_is_listened_on_through_a_copy() {
    let root = root("ok");
    let socket = root.join("blaze.sock");
    let passed = UnixListener::bind(&socket).unwrap();
    let listener = listen(&root, &inherited(passed.as_raw_fd())).unwrap();
    assert!(listener.is_unix());
    assert_ne!(listener.fd(), passed.as_raw_fd());
    // SAFETY: no pointers.
    let flags = unsafe { libc::fcntl(listener.fd(), libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0, "the copy is close-on-exec");
    let _client = UnixStream::connect(&socket).unwrap();
    assert!(listener.accept().is_ok());
    drop(listener);
    assert!(socket.exists(), "the manager's file is never removed");
    assert!(UnixStream::connect(&socket).is_ok(), "the passed descriptor stays open");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_inherited_socket_elsewhere_or_of_another_kind_is_refused() {
    let root = root("bad");
    let elsewhere = UnixListener::bind(root.join("other.sock")).unwrap();
    let said = refused(&root, &inherited(elsewhere.as_raw_fd()));
    assert!(said.contains("other.sock") && said.contains("not there"), "{said}");
    let (end, _peer) = UnixStream::pair().unwrap();
    assert!(refused(&root, &inherited(end.as_raw_fd())).contains("not listening"));
    let dir = std::fs::File::open(&root).unwrap();
    assert!(refused(&root, &inherited(dir.as_raw_fd())).contains("not a socket"));
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    assert!(refused(&root, &inherited(tcp.as_raw_fd())).contains("not a Unix socket"));
    assert!(refused(&root, &inherited(1 << 20)).contains("not open"));
    assert!(!root.join("blaze.sock").exists(), "nothing is made in its place");
    std::fs::remove_dir_all(&root).unwrap();
}

/// What a read appends is what std's safe read of the same stream gives, after the bytes the
/// buffer kept; a buffer with room is not grown, and none is shrunk.
#[test]
fn a_read_appends_after_what_the_buffer_keeps_as_a_safe_read_would() {
    use std::io::{Read as _, Write as _};

    let sent = b"onetwothree";
    let (ours, mut peer) = UnixStream::pair().unwrap();
    let (mut theirs, mut reference) = UnixStream::pair().unwrap();
    let ours = Socket::Unix(ours);
    peer.write_all(sent).unwrap();
    reference.write_all(sent).unwrap();
    let mut into = Vec::with_capacity(64);
    into.extend_from_slice(b"kept");
    let room = into.capacity();
    let mut safe = [0; 5];
    theirs.read_exact(&mut safe).unwrap();
    let mut got = 0;
    while got < 5 {
        got += ours.read_into(&mut into, 5 - got).unwrap();
    }
    assert_eq!(&into[..4], b"kept");
    assert_eq!(&into[4..], &safe);
    assert_eq!(into.capacity(), room, "a buffer with room is not grown");
    let mut rest = Vec::new();
    drop(peer);
    while ours.read_into(&mut rest, 2).unwrap() > 0 {}
    let mut safe_rest = Vec::new();
    drop(reference);
    theirs.read_to_end(&mut safe_rest).unwrap();
    assert_eq!(rest, safe_rest);
    let (len, room) = (rest.len(), rest.capacity());
    assert_eq!(ours.read_into(&mut rest, 8).unwrap(), 0, "the peer hung up");
    assert_eq!(rest.len(), len, "nothing is appended once the peer hung up");
    assert!(rest.capacity() >= room, "a capacity never shrinks");
}

/// A read that asks for more than [`crate::RECV_MOST`] reserves only that much room.
#[test]
fn a_read_of_any_most_reserves_at_most_recv_most() {
    use std::io::Write as _;

    let (ours, mut peer) = UnixStream::pair().unwrap();
    let ours = Socket::Unix(ours);
    peer.write_all(b"x").unwrap();
    let mut into = Vec::new();
    assert_eq!(ours.read_into(&mut into, usize::MAX).unwrap(), 1);
    assert_eq!(into, b"x");
    assert!(into.capacity() <= 1 + crate::RECV_MOST, "{}", into.capacity());
}

/// A listener dropped after the next one took its name (a daemon slow to end after it hung up,
/// as the next one started) leaves the next one's socket: only the file it made is removed.
#[test]
fn a_listener_dropped_after_another_took_its_name_leaves_that_ones_socket() {
    let root = root("taken");
    let at = Address::Unix(FileName::new("blaze.sock").unwrap());
    let socket = root.join("blaze.sock");
    let old = listen(&root, &at).unwrap();
    // The next daemon's start unlinks the name it finds no one answering on, then binds.
    std::fs::remove_file(&socket).unwrap();
    let new = listen(&root, &at).unwrap();
    drop(old);
    assert!(socket.exists(), "the old listener removed the new one's socket");
    UnixStream::connect(&socket).expect("the new listener is reached at the name");
    drop(new);
    assert!(!socket.exists(), "a listener removes the socket it made");
}
