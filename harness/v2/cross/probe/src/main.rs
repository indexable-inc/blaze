//! What a cross-built program is run for on its target (docs/linking.md, "Cross compilation"):
//! five checks of std over the target's C library, each printed as `ok <name>`, then
//! `cross-probe: 5 of 5`. Any failure panics, so the exit status says it. Pure Rust: it links
//! nothing but what blaze ships for the target.

use std::backtrace::Backtrace;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

fn hello() {
    let args: Vec<String> = std::env::args().collect();
    assert!(!args.is_empty(), "no argv[0]");
    println!("hello from {}-{}", std::env::consts::ARCH, std::env::consts::OS);
}

fn threads() {
    let (send, receive) = mpsc::channel();
    let total = Arc::new(Mutex::new(0u64));
    let handles: Vec<_> = (0..8u64)
        .map(|n| {
            let (send, total) = (send.clone(), Arc::clone(&total));
            std::thread::Builder::new()
                .name(format!("probe-{n}"))
                .spawn(move || {
                    thread_local!(static SEEN: std::cell::Cell<u64> = const { std::cell::Cell::new(7) });
                    SEEN.with(|it| it.set(it.get() + n));
                    *total.lock().unwrap() += SEEN.with(std::cell::Cell::get);
                    send.send(std::thread::current().name().unwrap().to_owned()).unwrap();
                })
                .unwrap()
        })
        .collect();
    drop(send);
    for it in handles {
        it.join().unwrap();
    }
    let mut names: Vec<String> = receive.iter().collect();
    names.sort();
    assert_eq!(names.len(), 8);
    assert_eq!(names[3], "probe-3");
    assert_eq!(*total.lock().unwrap(), 8 * 7 + 28);
}

fn file() {
    let dir = std::env::temp_dir().join(format!("cross-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data");
    let text: String = (0..4096).map(|n| char::from(b'a' + (n % 26) as u8)).collect();
    std::fs::write(&path, &text).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    let listed: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|it| it.unwrap().file_name()).collect();
    assert_eq!(listed, ["data"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

fn tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut got = [0u8; 5];
        peer.read_exact(&mut got).unwrap();
        got.reverse();
        peer.write_all(&got).unwrap();
    });
    let mut client = TcpStream::connect(at).unwrap();
    client.write_all(b"blaze").unwrap();
    let mut back = [0u8; 5];
    client.read_exact(&mut back).unwrap();
    server.join().unwrap();
    assert_eq!(&back, b"ezalb");
}

static DROPPED: AtomicBool = AtomicBool::new(false);

struct Flag;

impl Drop for Flag {
    fn drop(&mut self) {
        DROPPED.store(true, Ordering::SeqCst);
    }
}

#[inline(never)]
fn cross_probe_deepest(n: u32) -> u32 {
    let _flag = Flag;
    if n > 2 {
        panic!("probe panic {n}");
    }
    n
}

fn panic_with_backtrace() {
    let trace: Arc<Mutex<Option<String>>> = Arc::default();
    let kept = Arc::clone(&trace);
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |_| {
        *kept.lock().unwrap() = Some(Backtrace::force_capture().to_string());
    }));
    let caught = std::panic::catch_unwind(|| cross_probe_deepest(std::hint::black_box(3)));
    std::panic::set_hook(before);
    let payload = caught.expect_err("the panic did not unwind to catch_unwind");
    assert_eq!(payload.downcast_ref::<String>().map(String::as_str), Some("probe panic 3"));
    assert!(DROPPED.load(Ordering::SeqCst), "unwinding ran no destructor");
    let trace = trace.lock().unwrap().take().expect("the panic hook did not run");
    let frames = trace.lines().filter(|it| it.trim_start().split(':').next().is_some_and(|n| n.parse::<u32>().is_ok())).count();
    assert!(frames >= 3, "a backtrace of {frames} frames:\n{trace}");
    assert!(
        trace.contains("cross_probe_deepest"),
        "the backtrace does not name cross_probe_deepest:\n{trace}"
    );
}

fn main() {
    let checks: [(&str, fn()); 5] = [
        ("hello", hello),
        ("threads", threads),
        ("file", file),
        ("tcp", tcp),
        ("panic-backtrace", panic_with_backtrace),
    ];
    for (name, check) in checks {
        check();
        println!("ok {name}");
    }
    println!("cross-probe: {} of {}", checks.len(), checks.len());
}
