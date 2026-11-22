use std::{
    os::{
        fd::AsRawFd as _,
        unix::net::{UnixListener, UnixStream},
    },
};

use super::{Socket, listen, peer_judged};
use crate::{Address, FileName, temp::TempDir};

/// A directory for the test's sockets, removed when the guard drops.
fn root(name: &str) -> TempDir {
    TempDir::for_test_socket(&format!("bz-inherited-{name}-"))
}

fn inherited(fd: i32) -> Address {
    Address::Inherited { fd, at: FileName::new("blaze.sock").unwrap() }
}

fn refused(root: &std::path::Path, at: &Address) -> String {
    match listen(root, at, &crate::sockopt::Tcp::DEFAULT) {
        Ok(_) => panic!("{at:?} was listened on"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn an_inherited_socket_bound_at_its_name_is_listened_on_through_a_copy() {
    let root = root("ok");
    let socket = root.join("blaze.sock");
    let passed = UnixListener::bind(&socket).unwrap();
    let listener = listen(&root, &inherited(passed.as_raw_fd()), &crate::sockopt::Tcp::DEFAULT).unwrap();
    assert!(listener.is_unix());
    assert_ne!(listener.fd(), passed.as_raw_fd());
    let closes = blaze_sys::posix::io::close_on_exec(blaze_sys::Fd::new(listener.fd()));
    // F_GETFD's -1 error sentinel also had the close-on-exec bit set.
    assert!(closes.unwrap_or(true), "the copy is close-on-exec");
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
    let old = listen(&root, &at, &crate::sockopt::Tcp::DEFAULT).unwrap();
    // The next daemon's start unlinks the name it finds no one answering on, then binds.
    std::fs::remove_file(&socket).unwrap();
    let new = listen(&root, &at, &crate::sockopt::Tcp::DEFAULT).unwrap();
    drop(old);
    assert!(socket.exists(), "the old listener removed the new one's socket");
    UnixStream::connect(&socket).expect("the new listener is reached at the name");
    drop(new);
    assert!(!socket.exists(), "a listener removes the socket it made");
}

/// A connection of another user is refused at its accept, before any byte of it is read
/// (`peer_is`), and the listener goes on: the next connection of its own user is taken and
/// read. No second user exists where tests run, so the listener is told another uid
/// (`answer_only`) and this process plays the foreign peer. The control is the same connection
/// told to a listener of this process's own uid: taken, its byte read.
#[test]
fn a_connection_of_another_user_is_refused_before_a_byte_is_read() {
    use std::io::{Read as _, Write as _};

    let root = root("peer");
    let at = Address::Unix(FileName::new("blaze.sock").unwrap());
    let socket = root.join("blaze.sock");
    let own = blaze_sys::posix::thread::geteuid();
    let mut listener = listen(&root, &at, &crate::sockopt::Tcp::DEFAULT).unwrap();

    // Foreign: the listener answers another uid alone.
    listener.answer_only(own.wrapping_add(1));
    let mut foreign = UnixStream::connect(&socket).unwrap();
    // The read's timeout is set while the socket is connected: set after the refusal closed
    // the other end, macOS 27 refused it with EINVAL (wave-1119 t60 core-net.txt, the old
    // tests.rs:159, after the refusal's assertions above had passed).
    foreign.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    foreign.write_all(b"x").unwrap();
    let refused = match listener.accept() {
        Ok(_) => panic!("a connection of uid {own} was taken by a listener answering uid {} alone", own.wrapping_add(1)),
        Err(error) => error,
    };
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionAborted, "{refused}");
    let said = refused.to_string();
    assert!(said.contains(&format!("uid {own}")) && said.contains(&format!("uid {}", own.wrapping_add(1))), "the refusal names both users: {said}");
    // Closed unread: the peer sees its end, no answer.
    let mut answer = [0u8; 1];
    match foreign.read(&mut answer) {
        Ok(0) => {}
        Ok(_) => panic!("the foreign peer was answered"),
        Err(error) => assert_ne!(error.kind(), std::io::ErrorKind::WouldBlock, "the refused connection was left open"),
    }

    // Control: the same connection to a listener of this process's own uid is taken and read.
    listener.answer_only(own);
    let mut ours = UnixStream::connect(&socket).unwrap();
    ours.write_all(b"y").unwrap();
    let Socket::Unix(mut taken) = listener.accept().expect("a connection of the listener's own user is taken") else {
        panic!("a Unix listener took a TCP connection");
    };
    let mut got = [0u8; 1];
    taken.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"y");
}

/// A peer whose uid cannot be read (gone before its accept, where the kernel keeps none) is
/// refused like a foreign one: `ConnectionAborted`, naming why and the uid the socket answers,
/// never taken. CONTROL: the same decision on a readable uid of the socket's own user takes it.
#[test]
fn a_peer_whose_uid_cannot_be_read_is_refused_not_taken() {
    let refused = peer_judged(Err(blaze_sys::Errno::Inval), 501).expect_err("a peer of no readable uid was taken");
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionAborted, "{refused}");
    let said = refused.to_string();
    assert!(said.contains("could not be read") && said.contains("uid 501"), "the refusal says why and whom the socket answers: {said}");
    assert!(peer_judged(Ok(501), 501).is_ok(), "CONTROL: the socket's own user is taken");
    assert_eq!(peer_judged(Ok(502), 501).map_err(|error| error.kind()), Err(std::io::ErrorKind::ConnectionAborted), "another user is refused");
}
