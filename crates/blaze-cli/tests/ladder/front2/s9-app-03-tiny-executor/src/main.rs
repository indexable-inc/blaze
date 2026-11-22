//! s9-app-03: tokio's shape in sixty lines: a current-thread executor with a ready queue of
//! `Arc<Task>` woken through `std::task::Wake`, `Pin<Box<dyn Future + Send>>` tasks, and a
//! virtual clock: `sleep(n)` registers its waker under (deadline, sequence number) in a
//! `BTreeMap`; when the ready queue is empty the clock jumps to the first deadline and that one
//! waker is woken. Nothing reads the real time, so the order is fixed by the program:
//! a ticks every 3 (twice), b every 2 (three times), c once at 0. Polls: a 3, b 4, c 1.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

type BoxFut = Pin<Box<dyn Future<Output = ()> + Send>>;
type Queue = Arc<Mutex<VecDeque<Arc<Task>>>>;

struct Task {
    fut: Mutex<Option<BoxFut>>,
    queue: Queue,
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        let q = self.queue.clone();
        q.lock().unwrap().push_back(self);
    }
}

struct Timers {
    now: u64,
    waiting: BTreeMap<(u64, usize), Waker>,
    seq: usize,
}

static TIMERS: Mutex<Timers> = Mutex::new(Timers { now: 0, waiting: BTreeMap::new(), seq: 0 });
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Sleep {
    until: u64,
    registered: bool,
}

fn sleep(ticks: u64) -> Sleep {
    let now = TIMERS.lock().unwrap().now;
    Sleep { until: now + ticks, registered: false }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut t = TIMERS.lock().unwrap();
        if t.now >= self.until {
            return Poll::Ready(());
        }
        if !self.registered {
            t.seq += 1;
            let key = (self.until, t.seq);
            t.waiting.insert(key, cx.waker().clone());
            drop(t);
            self.registered = true;
        }
        Poll::Pending
    }
}

fn log(what: String) {
    let now = TIMERS.lock().unwrap().now;
    LOG.lock().unwrap().push(format!("t={now} {what}"));
}

async fn worker(name: &'static str, period: u64, times: u32) {
    for i in 0..times {
        sleep(period).await;
        log(format!("{name} tick {i}"));
    }
    log(format!("{name} done"));
}

fn main() {
    let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
    let spawn = |f: BoxFut| {
        let task = Arc::new(Task { fut: Mutex::new(Some(f)), queue: queue.clone() });
        queue.lock().unwrap().push_back(task);
    };
    spawn(Box::pin(worker("a", 3, 2)));
    spawn(Box::pin(worker("b", 2, 3)));
    spawn(Box::pin(async { log(String::from("c immediate")) }));
    let mut polls = 0;
    loop {
        let next = queue.lock().unwrap().pop_front();
        if let Some(task) = next {
            polls += 1;
            let waker = Waker::from(task.clone());
            let mut cx = Context::from_waker(&waker);
            let mut slot = task.fut.lock().unwrap();
            if let Some(mut f) = slot.take() {
                if f.as_mut().poll(&mut cx).is_pending() {
                    *slot = Some(f);
                }
            }
            continue;
        }
        let woken = {
            let mut t = TIMERS.lock().unwrap();
            let Some(&(deadline, seq)) = t.waiting.keys().next() else { break };
            t.now = deadline;
            t.waiting.remove(&(deadline, seq))
        };
        if let Some(w) = woken {
            w.wake();
        }
    }
    for line in LOG.lock().unwrap().iter() {
        println!("{line}");
    }
    println!("polls {polls}, clock {}", TIMERS.lock().unwrap().now);
}
