// cc 1.4.3's in-process jobserver and executor (src/parallel/{job_token,async_executor}.rs),
// each step on its own, every loop bounded so a wrong build prints instead of hanging.

use std::{
    future::Future,
    hint::black_box,
    pin::Pin,
    ptr,
    sync::{
        OnceLock,
        atomic::{
            AtomicU32,
            Ordering::{AcqRel, Acquire},
        },
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

const NOOP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(|_| NOOP_RAW_WAKER, |_| {}, |_| {}, |_| {});
const NOOP_RAW_WAKER: RawWaker = RawWaker::new(ptr::null(), &NOOP_WAKER_VTABLE);

#[derive(Default)]
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let flag = &mut Pin::into_inner(self).0;
        if !*flag {
            *flag = true;
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

async fn acquire(tokens: &AtomicU32) -> u32 {
    loop {
        #[allow(deprecated)]
        let res = tokens.fetch_update(AcqRel, Acquire, |tokens| tokens.checked_sub(1));
        if let Ok(had) = res {
            break had;
        }
        YieldOnce::default().await
    }
}

/// `fut` polled at most `bound` times on cc's no-op waker: its output, and how many polls.
fn run<F: Future>(fut: F, bound: u32) -> (Option<F::Output>, u32) {
    let mut fut = std::pin::pin!(fut);
    let waker = unsafe { Waker::from_raw(NOOP_RAW_WAKER) };
    let mut context = Context::from_waker(&waker);
    for polls in 1..=bound {
        if let Poll::Ready(it) = fut.as_mut().poll(&mut context) {
            return (Some(it), polls);
        }
    }
    (None, bound)
}

fn report(jobs: &str) -> String {
    let parsed = jobs.parse::<u32>().map(|it| it > 0);
    let sub = black_box(18u32).checked_sub(1);
    let zero = black_box(0u32).checked_sub(1);
    let atomic = AtomicU32::new(black_box(18));
    #[allow(deprecated)]
    let update = atomic.fetch_update(AcqRel, Acquire, |it| it.checked_sub(1));
    let after_update = atomic.load(Acquire);
    let exchange = atomic.compare_exchange(17, 16, AcqRel, Acquire);
    let after_exchange = atomic.load(Acquire);
    let yielded = run(
        async {
            YieldOnce::default().await;
            7
        },
        100,
    );
    static SERVER: OnceLock<AtomicU32> = OnceLock::new();
    let server = SERVER.get_or_init(|| AtomicU32::new(black_box(18)));
    let acquired = run(acquire(server), 100);
    let drained = AtomicU32::new(black_box(0));
    let none_left = run(acquire(&drained), 100);
    format!(
        "parse {parsed:?} sub {sub:?} zero {zero:?} update {update:?} left {after_update} \
         exchange {exchange:?} left {after_exchange} yield {yielded:?} acquire {acquired:?} \
         none left {none_left:?} released {}",
        released()
    )
}

static TOKENS: AtomicU32 = AtomicU32::new(0);

/// cc's `JobToken`: zero-sized, its drop gives the token back.
struct Token;

impl Drop for Token {
    fn drop(&mut self) {
        TOKENS.fetch_add(1, AcqRel);
    }
}

/// What cc's wait loop does with finished jobs: `retain_mut` removes them, dropping each
/// `(Command, KillOnDrop, JobToken)`. How many tokens came back of the three removed.
fn released() -> u32 {
    let mut pending = vec![(String::from("a"), Token), (String::from("b"), Token), (String::from("c"), Token)];
    pending.retain_mut(|(name, _)| black_box(name.as_str()) == "keep");
    TOKENS.load(Acquire)
}
