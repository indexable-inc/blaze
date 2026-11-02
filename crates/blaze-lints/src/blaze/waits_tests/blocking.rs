//! `blocking_on_pool` over [`super`]'s fixtures, with a `parking_lot` and a `crossbeam_channel`
//! of their own.

use super::check_in;

const CRATES: &str = r#"
//- /parking_lot.rs crate:parking_lot
pub struct Condvar;
impl Condvar {
    pub fn wait<T>(&self, _guard: &mut T) {}
    pub fn notify_all(&self) {}
}
//- /crossbeam_channel.rs crate:crossbeam_channel
pub struct Receiver<T>(*mut T);
impl<T> Receiver<T> {
    pub fn recv(&self) -> Option<T> {
        None
    }
    pub fn try_recv(&self) -> Option<T> {
        None
    }
}
//- /src/lib.rs crate:main deps:engine,std,blaze_graph,parking_lot,crossbeam_channel
"#;

/// On a job the fiber driver runs, and in a function that may suspend: a `Condvar` wait (std's,
/// `parking_lot`'s), a receive (std's through a re-export, `crossbeam_channel`'s), a join, a
/// sleep through a renamed import, a park, a spin, and the IO of `std::fs` and `std::net`
/// (a macro's included) are findings; their non-blocking neighbours (`try_recv`, `notify_one`,
/// `File::options`, `OpenOptions::read`, `local_addr`), and the same calls in a function off the
/// pool, are none.
#[test]
fn blocking_on_pool() {
    let out = check_in(
        CRATES,
        r#"
use engine::Db;
use std::io::Read as _;
use std::sync::{Condvar, Mutex};
use std::thread::sleep as nap;

mod inbox {
    pub use std::sync::mpsc::Receiver as Inbox;
}

pub struct Shared {
    lock: Mutex<u32>,
    ready: Condvar,
    lot: parking_lot::Condvar,
    rx: inbox::Inbox<u32>,
    beam: crossbeam_channel::Receiver<u32>,
}

macro_rules! nap_a_while {
    () => {
        nap(1)
    };
}

pub fn submit(shared: &'static Shared) {
    engine::fiber::spawn(move || job(shared));
}

fn job(shared: &Shared) {
    let guard = shared.lock.lock();
    let _guard = shared.ready.wait(guard); //~ blocking_on_pool
    shared.ready.notify_one();
    let mut n = 0;
    shared.lot.wait(&mut n); //~ blocking_on_pool
    shared.lot.notify_all();
    let _ = shared.rx.recv(); //~ blocking_on_pool
    let _ = shared.rx.try_recv();
    let _ = shared.beam.recv(); //~ blocking_on_pool
    let _ = shared.beam.try_recv();
    nap(1); //~ blocking_on_pool
    std::thread::park(); //~ blocking_on_pool
    std::hint::spin_loop(); //~ blocking_on_pool
    nap_a_while!(); //~ blocking_on_pool
    let _ = std::fs::read("a"); //~ blocking_on_pool
    let mut file = std::fs::File::open("a"); //~ blocking_on_pool
    let mut buf = [0u8; 4];
    let _ = file.read(&mut buf); //~ blocking_on_pool
    let mut options = std::fs::File::options();
    let _ = options.read(true).open("a"); //~ blocking_on_pool
    let stream = std::net::TcpStream::connect("a"); //~ blocking_on_pool
    let _ = stream.local_addr();
}

pub fn joins(handle: std::thread::JoinHandle<u32>) -> u32 {
    handle.join()
}

pub fn suspends(db: &Db, shared: &Shared, handle: std::thread::JoinHandle<u32>) -> u32 {
    let _ = shared.rx.recv(); //~ blocking_on_pool
    let _ = handle.join(); //~ blocking_on_pool
    db.variance(1)
}

pub fn off_the_pool(shared: &Shared) {
    let _ = shared.rx.recv();
    nap(1);
    let _ = std::fs::read("a");
}
"#,
        "-A clippy::all --warn=blaze::blocking_on_pool",
    );
    assert!(out.contains("`Condvar::wait` blocks the pool thread `job` runs on"), "{out}");
    assert!(out.contains("`std::thread::sleep` blocks the pool thread `job` runs on"), "{out}");
    assert!(out.contains("`Read::read` on `File` blocks the pool thread"), "{out}");
    assert!(out.contains("`std::fs::read` blocks the pool thread"), "{out}");
    assert!(out.contains("`blaze_graph::sched::wait(Wait::Until(deadline))`"), "the sleep says what to wait with:\n{out}");
}
