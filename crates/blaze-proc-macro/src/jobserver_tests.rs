//! The jobserver's promises: `workers - 1` tokens at rest in a 0600 fifo, a child that exits
//! holding tokens costs none (they come back by count once no run is running), and the fifo is
//! gone at the command's end.

use core::num::NonZeroU32;
use std::{
    fs::OpenOptions,
    io::{ErrorKind, Read, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
};

use super::{FOLDER, Jobserver, JobserverOpts, Returned, TOKEN};

fn work(name: &str) -> std::string::String {
    let dir = std::env::temp_dir().join(format!("blaze-jobserver-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a test folder");
    dir.canonicalize().expect("its real path").to_str().expect("a UTF-8 temp path").to_owned()
}

/// A client as the `jobserver` crate opens `fifo:PATH`: read and write.
fn client(path: &str) -> std::fs::File {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path).expect("the fifo opens as a client")
}

/// Takes up to `most` tokens without blocking.
fn take(file: &mut std::fs::File, most: usize) -> usize {
    let mut got = 0;
    let mut byte = [0u8; 1];
    while got < most {
        match file.read(&mut byte) {
            Ok(1) => {
                assert_eq!(byte[0], TOKEN);
                got += 1;
            }
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("{error}"),
        }
    }
    got
}

/// Four workers: three tokens at rest, fifo mode 0600 in a 0700 folder. A child that takes two
/// and exits keeping them (killed) gives them back by count when the run ends; one that returns
/// what it took gives back nothing more. While another run is still running nothing is counted
/// (its child may hold tokens). Control: without the give-back the second child would find one
/// token, not three (asserted by taking before the end). At the end the fifo is gone.
/// Prints "jobserver: 3 tokens at rest, 2 kept by a killed child returned by count, fifo removed".
#[test]
fn jobserver_returns_a_dead_childs_tokens_by_count_and_is_its_owners_alone() {
    let work = work("count");
    let server = Jobserver::new(JobserverOpts { work: &work, workers: NonZeroU32::new(4).expect("4"), id: 7 }).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(server.fifo(), format!("{work}/{FOLDER}/7"));
    assert_eq!(server.tokens(), 3);
    let meta = std::fs::metadata(server.fifo()).expect("the fifo");
    assert!(meta.file_type().is_fifo());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(format!("{work}/{FOLDER}")).expect("its folder").permissions().mode() & 0o777, 0o700);

    // A run whose child takes two tokens and is killed holding them.
    let lent = server.lend().unwrap_or_else(|e| panic!("{e}"));
    let other = server.lend().unwrap_or_else(|e| panic!("{e}"));
    {
        let mut child = client(server.fifo());
        assert_eq!(take(&mut child, 2), 2);
    }
    // Another run is still running: nothing is counted yet.
    assert_eq!(lent.end().unwrap_or_else(|e| panic!("{e}")), Returned { kept: 0 });
    {
        let mut peek = client(server.fifo());
        let left = take(&mut peek, 8);
        assert_eq!(left, 1, "the control: one token left while the killed child's two are out");
        peek.write_all(&[TOKEN]).expect("the peeked token back");
    }
    // The last run ends: the two come back by count.
    assert_eq!(other.end().unwrap_or_else(|e| panic!("{e}")), Returned { kept: 2 });
    assert_eq!(server.restored().unwrap_or_else(|e| panic!("{e}")), 2);
    {
        let mut child = client(server.fifo());
        assert_eq!(take(&mut child, 8), 3, "all three tokens at rest again");
        child.write_all(&[TOKEN; 3]).expect("returned");
    }
    // A child that gives back what it took: nothing is restored.
    let lent = server.lend().unwrap_or_else(|e| panic!("{e}"));
    {
        let mut child = client(server.fifo());
        assert_eq!(take(&mut child, 3), 3);
        child.write_all(&[TOKEN; 3]).expect("returned");
    }
    assert_eq!(lent.end().unwrap_or_else(|e| panic!("{e}")), Returned { kept: 0 });
    assert_eq!(server.restored().unwrap_or_else(|e| panic!("{e}")), 2);

    server.end().unwrap_or_else(|e| panic!("{e}"));
    assert!(std::fs::metadata(server.fifo()).is_err(), "the fifo is removed at the end");
    assert!(server.lend().is_err(), "no run is lent after the end");
    println!("jobserver: 3 tokens at rest, 2 kept by a killed child returned by count, fifo removed");
    _ = std::fs::remove_dir_all(&work);
}

/// A stale fifo of a dead command with the same id is replaced, not reused (its tokens are
/// unknown); one worker gives no tokens at all (the script's implicit token is its worker).
#[test]
fn jobserver_replaces_a_stale_fifo_and_one_worker_lends_nothing() {
    let work = work("stale");
    let first = Jobserver::new(JobserverOpts { work: &work, workers: NonZeroU32::new(3).expect("3"), id: 9 }).unwrap_or_else(|e| panic!("{e}"));
    let path = first.fifo().to_owned();
    // The dead command never ended its jobserver; a new one of the same id starts afresh.
    let second = Jobserver::new(JobserverOpts { work: &work, workers: NonZeroU32::MIN, id: 9 }).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(second.fifo(), path);
    assert_eq!(second.tokens(), 0);
    let mut child = client(second.fifo());
    assert_eq!(take(&mut child, 8), 0);
    second.end().unwrap_or_else(|e| panic!("{e}"));
    drop(first);
    _ = std::fs::remove_dir_all(&work);
}
