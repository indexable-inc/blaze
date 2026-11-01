//! The asks between two ends of a real channel in one process: the daemon's [`Reader`] and
//! [`Sender`] on a view of an asks object, the run's [`request`] on the same view.

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use blaze_ids::rng::SplitMix64;

use super::*;
use crate::Access;

fn temp_dir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!("bza-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

fn asks_for(pid: u32) -> Run {
    let object = sys::Object::create("the asks", BYTES.next_multiple_of(sys::page())).unwrap();
    let words = object.map_here(Access::Shared).unwrap();
    prepare(&words);
    let remote = sys::Remote::of(pid).unwrap();
    let (bells, names) = sys::Bells::new(&remote, SLOTS).unwrap();
    name_bells(&words, &names);
    Run::new(words, bells, remote, pid)
}

/// Both ends: the run's `hook` end of the channel and the daemon's reader and sender, of this
/// process, which no test kills.
struct Both {
    reader: Reader,
    sender: Sender,
    run: Arc<Run>,
    hook: sys::Stream,
}

fn both() -> Both {
    let dir = temp_dir();
    let (listener, address) = sys::Listener::bind(&dir).unwrap();
    let hook = sys::connect(&address).unwrap();
    let (daemon, pid) = listener.accept().unwrap();
    drop(listener);
    let _ = std::fs::remove_dir(&dir);
    let run = Arc::new(asks_for(pid));
    Both { reader: Reader::new(Channel::new(daemon), run.clone()), sender: Sender::new(run.clone()), run, hook }
}

/// Answers each ask of kind `k` with kind `k + 1` and its body reversed, until the run hangs
/// up: how many it answered.
fn echo(mut reader: Reader, sender: Sender) -> std::thread::JoinHandle<usize> {
    std::thread::spawn(move || {
        let mut answered = 0;
        while let Ok(frame) = reader.recv() {
            let mut body = frame.body.to_vec();
            body.reverse();
            sender.send(Kind(frame.kind.0 + 1), frame.seq, &body).unwrap();
            answered += 1;
        }
        answered
    })
}

/// Aborts the test process unless dropped within a minute: a deadlock fails instead of hanging.
struct Watchdog {
    /// Dropped with the watchdog, which ends its thread's wait.
    _done: crossbeam_channel::Sender<()>,
}

impl Watchdog {
    fn new(what: &'static str) -> Watchdog {
        let (done, seen) = crossbeam_channel::bounded::<()>(0);
        std::thread::spawn(move || {
            if seen.recv_timeout(Duration::from_secs(60)) == Err(crossbeam_channel::RecvTimeoutError::Timeout) {
                eprintln!("{what}: deadlocked");
                std::process::abort();
            }
        });
        Watchdog { _done: done }
    }
}

#[test]
fn an_asker_spins_about_one_wake_up_and_no_longer() {
    let words: Vec<AtomicU64> = (0..WORDS).map(|_| AtomicU64::new(0)).collect();
    prepare(&words);
    let index = claim(&words).unwrap();
    let generation = ask(&words, index, Kind(7), b"x");
    let start = Instant::now();
    assert_eq!(answered_within(&words, index, generation, ASKER_SPIN), Err(state(generation, ASKED)));
    let spun = start.elapsed();
    assert!(spun >= ASKER_SPIN && spun < Duration::from_millis(100), "{spun:?}");
    assert_eq!(take(&words, 0, &mut Vec::new()).unwrap().map(|it| it.0), Some(index));
    answer_into(&words, seq(index, generation), Kind(8), b"y").unwrap();
    let start = Instant::now();
    assert_eq!(answered_within(&words, index, generation, Duration::from_secs(60)), Ok(()));
    assert!(start.elapsed() < Duration::from_secs(1), "an answer already there ends the spin at once");
}

#[test]
fn many_threads_asking_at_once_each_get_their_own_answer() {
    const THREADS: usize = 2 * SLOTS;
    const ASKS: usize = 40;
    let _watchdog = Watchdog::new("many threads asking");
    let Both { reader, sender, run, hook } = both();
    let daemon = echo(reader, sender);
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let (run, hook) = (&run, &hook);
            scope.spawn(move || {
                let mut rng = SplitMix64::new(thread as u64);
                let mut out = [0u8; MOST_BODY];
                for n in 0..ASKS as u16 {
                    let len = rng.index(MOST_BODY + 1);
                    let body: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
                    let answered = request(&run.words, hook, Kind(0x200 + 2 * n), &body, &mut out).unwrap();
                    assert_eq!(answered, (Kind(0x201 + 2 * n), len));
                    assert!(out[..len].iter().eq(body.iter().rev()), "thread {thread} got another's answer");
                }
            });
        }
    });
    drop(hook);
    assert_eq!(daemon.join().unwrap(), THREADS * ASKS);
    assert_eq!(Words::word(&run.words, W_USED).load(Ordering::SeqCst), 0, "a slot was never given back");
}

/// The asks a signal handler makes: the test's one run, for the handler to find.
struct Asker {
    run: Arc<Run>,
    hook: sys::Stream,
}

static ASKER: OnceLock<Asker> = OnceLock::new();
/// 1 once the handler's ask was answered as it should be, 2 otherwise.
static HANDLED: AtomicU32 = AtomicU32::new(0);

extern "C" fn asks_inside(_: libc::c_int) {
    let Some(asker) = ASKER.get() else { return };
    let mut out = [0u8; 16];
    let got = request(&asker.run.words, &asker.hook, Kind(0x300), b"handler", &mut out);
    let right = matches!(got, Ok((Kind(0x301), 7))) && &out[..7] == b"reldnah";
    HANDLED.store(if right { 1 } else { 2 }, Ordering::SeqCst);
}

#[test]
fn a_signal_handler_that_asks_inside_its_threads_ask_is_answered_first() {
    let _watchdog = Watchdog::new("a signal handler asking");
    let Both { mut reader, sender, run, hook } = both();
    let asker = ASKER.get_or_init(|| Asker { run, hook });
    // SAFETY: a zeroed `sigaction` is a valid one (no flags, an empty mask); the handler is an
    // `extern "C" fn(c_int)`, which is what `sa_sigaction` holds without SA_SIGINFO.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = asks_inside as extern "C" fn(libc::c_int) as usize;
        assert_eq!(libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()), 0);
    }
    let (thread_tx, thread_rx) = crossbeam_channel::unbounded();
    let thread = std::thread::spawn(move || {
        // SAFETY: the calling thread's own handle.
        thread_tx.send(unsafe { libc::pthread_self() }).unwrap();
        let mut out = [0u8; 16];
        let got = request(&asker.run.words, &asker.hook, Kind(0x200), b"thread", &mut out);
        (got.map_err(|it| it.what), out)
    });
    let pthread = thread_rx.recv().unwrap();
    let first = {
        let frame = reader.recv().unwrap();
        assert_eq!((frame.kind, frame.body), (Kind(0x200), &b"thread"[..]));
        frame.seq
    };
    // SAFETY: the thread is alive: it waits for the answer to `first`, which nothing sent.
    assert_eq!(unsafe { libc::pthread_kill(pthread, libc::SIGUSR1) }, 0);
    let second = {
        let frame = reader.recv().unwrap();
        assert_eq!((frame.kind, frame.body), (Kind(0x300), &b"handler"[..]));
        frame.seq
    };
    sender.send(Kind(0x301), second, b"reldnah").unwrap();
    blaze_spin::SpinWait::within("the handler's answer", Duration::from_secs(60), || HANDLED.load(Ordering::SeqCst) != 0);
    assert_eq!(HANDLED.load(Ordering::SeqCst), 1, "the handler's answer");
    sender.send(Kind(0x201), first, b"daerht").unwrap();
    let (got, out) = thread.join().unwrap();
    assert_eq!(got, Ok((Kind(0x201), 6)));
    assert_eq!(&out[..6], b"daerht");
}

#[test]
fn an_answer_longer_than_the_buffer_says_its_length_and_errno_is_kept() {
    let _watchdog = Watchdog::new("a long answer");
    let Both { mut reader, sender, run, hook } = both();
    let daemon = std::thread::spawn(move || {
        let seq = reader.recv().unwrap().seq;
        sender.send(Kind(1), seq, &[7; 100]).unwrap();
    });
    // SAFETY: this thread's `errno`.
    unsafe { *libc::__errno_location() = 77 };
    let mut out = [0u8; 10];
    assert_eq!(request(&run.words, &hook, Kind(0), b"", &mut out).unwrap(), (Kind(1), 100));
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(77));
    assert_eq!(out, [7; 10]);
    daemon.join().unwrap();
    let too_long = request(&run.words, &hook, Kind(0), &[0; MOST_BODY + 1], &mut out).unwrap_err();
    assert!(too_long.what.contains("past"), "{too_long}");
}

#[test]
fn an_answer_is_taken_once_and_only_for_its_own_ask() {
    let _watchdog = Watchdog::new("a stale answer");
    let Both { mut reader, sender, run, hook } = both();
    let asking = std::thread::spawn(move || {
        let mut out = [0u8; 8];
        request(&run.words, &hook, Kind(4), b"ask", &mut out).map_err(|it| it.what)
    });
    let seq = reader.recv().unwrap().seq;
    assert!(sender.send(Kind(5), seq ^ 1 << 8, b"").is_err(), "an answer of another generation");
    assert!(sender.send(Kind(5), seq | 0xff, b"").is_err(), "an answer to no slot");
    assert!(sender.send(Kind(5), seq, &[0; MOST_BODY + 1]).is_err(), "an answer past the slot");
    sender.send(Kind(5), seq, b"ok").unwrap();
    assert!(sender.send(Kind(5), seq, b"again").is_err(), "a second answer");
    assert_eq!(asking.join().unwrap(), Ok((Kind(5), 2)));
    assert!(sender.send(Kind(5), seq, b"late").is_err(), "an answer after the slot was given back");
}

#[test]
fn an_ask_ends_when_the_daemon_hangs_up() {
    let _watchdog = Watchdog::new("a hang-up");
    let Both { mut reader, sender, run, hook } = both();
    let asking = std::thread::spawn(move || {
        let mut out = [0u8; 8];
        request(&run.words, &hook, Kind(4), b"ask", &mut out).map_err(|it| it.what)
    });
    let _ = reader.recv().unwrap();
    drop((reader, sender));
    let error = asking.join().unwrap().unwrap_err();
    assert!(error.contains("hung up"), "{error}");
}

#[test]
fn an_ask_whose_length_is_past_its_slot_is_refused() {
    let Both { mut reader, run, hook, .. } = both();
    let index = claim(&run.words).unwrap();
    ask(&run.words, index, Kind(5), b"x");
    Words::word(&run.words, base(index) + S_ASK).store(5 | (MOST_BODY as u64 + 1) << 16, Ordering::Relaxed);
    sys::ring(&hook, 0).unwrap();
    let error = reader.recv().unwrap_err();
    assert!(error.what.contains("past"), "{error}");
}

#[test]
fn a_kill_ends_the_process_and_keeps_the_first_reason() {
    let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let run = asks_for(child.id());
    run.kill("first").unwrap();
    run.kill("second").unwrap();
    assert_eq!(run.killed().as_deref(), Some("first"));
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    run.kill("after it was reaped").unwrap();
}
