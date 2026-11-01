//! Running the async [`Registry`](crate::Registry) from the resolver's loop:
//! `src/util/local_poll_adapter.rs` of cargo 1.99.0 (5f94df4789), with an executor of its
//! own in place of the `futures` crate's.

use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;

use blaze_hash::HashMap;

/// Caches and runs an async method from code that is not async, on one thread.
///
/// `start` begins the work for a key: an `async fn` taking the `self_parameter` and the key
/// and handing the key back with its result, so every piece of work is the one future type
/// `Q`. The `self_parameter`, the key and successful results must be cheap to clone. At most
/// one computation per key is in flight; a success is kept, an error is handed out once.
pub(crate) struct LocalPollAdapter<S, K, R, Q> {
    pool: Vec<Pin<Box<Q>>>,
    cache: HashMap<K, Poll<R>>,
    self_parameter: S,
    start: fn(S, K) -> Q,
}

impl<S, K, V, E, Q> LocalPollAdapter<S, K, Result<V, E>, Q>
where
    S: Clone,
    K: Clone + Hash + Eq,
    V: Clone,
    Q: Future<Output = (K, Result<V, E>)>,
{
    pub(crate) fn new(self_parameter: S, start: fn(S, K) -> Q) -> Self {
        Self { pool: Vec::new(), cache: HashMap::default(), self_parameter, start }
    }

    /// The result for `key`, starting the work if needed. On [`Poll::Pending`], call
    /// [`LocalPollAdapter::wait`] and ask again. Work that finishes without waiting is not
    /// queued.
    pub(crate) fn poll(&mut self, key: K) -> Poll<Result<V, E>> {
        match self.cache.get(&key) {
            Some(Poll::Ready(Ok(v))) => return Poll::Ready(Ok(v.clone())),
            Some(Poll::Ready(Err(_))) => return self.cache.remove(&key).unwrap(),
            Some(Poll::Pending) => return Poll::Pending,
            None => {}
        }

        let mut future = Box::pin((self.start)(self.self_parameter.clone(), key.clone()));

        if let Poll::Ready((k, v)) = future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            if let Ok(success) = &v {
                self.cache.insert(k, Poll::Ready(Ok(success.clone())));
            }
            return Poll::Ready(v);
        }

        self.cache.insert(key, Poll::Pending);
        self.pool.push(future);
        Poll::Pending
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.pool.len()
    }

    /// Runs the pending work to its end. True if there was none.
    pub(crate) fn wait(&mut self) -> bool {
        let is_empty = self.pool.is_empty();
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        while !self.pool.is_empty() {
            let mut progressed = false;
            let mut i = 0;
            while i < self.pool.len() {
                if let Poll::Ready((k, v)) = self.pool[i].as_mut().poll(&mut cx) {
                    drop(self.pool.swap_remove(i));
                    *self.cache.get_mut(&k).expect("all pending work is in the cache") =
                        Poll::Ready(v);
                    progressed = true;
                } else {
                    i += 1;
                }
            }
            if !progressed {
                std::thread::park();
            }
        }
        is_empty
    }
}

/// `expect` for a [`Poll`] that must be ready.
pub(crate) trait PollExt<T> {
    fn expect(self, msg: &str) -> T;
}

impl<T> PollExt<T> for Poll<T> {
    #[track_caller]
    fn expect(self, msg: &str) -> T {
        match self {
            Poll::Ready(val) => val,
            Poll::Pending => panic!("{msg}"),
        }
    }
}

/// Runs `future` to its end on this thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

/// Wakes the thread that waits; a wake before it parks makes the park return at once.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::rc::Rc;

    use super::*;

    /// A future that is pending once, as a query waiting on the network is.
    pub(crate) fn yield_once() -> impl Future<Output = ()> {
        let mut yielded = false;
        std::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    /// A future woken from another thread, as one waiting on a download is. Each poll leaves
    /// its waker for the thread, as a future must.
    fn woken_from_elsewhere() -> impl Future<Output = ()> {
        let state = Arc::new(parking_lot::Mutex::new((false, None::<Waker>)));
        let mut started = false;
        std::future::poll_fn(move |cx| {
            let mut guard = state.lock();
            if guard.0 {
                return Poll::Ready(());
            }
            guard.1 = Some(cx.waker().clone());
            if !started {
                started = true;
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    blaze_spin::busy(std::time::Duration::from_millis(20));
                    let mut guard = state.lock();
                    guard.0 = true;
                    guard.1.take().unwrap().wake();
                });
            }
            Poll::Pending
        })
    }

    struct Thing {}

    impl Thing {
        async fn widen(&self, i: &i32) -> Result<i64, ()> {
            if *i > 1000 {
                woken_from_elsewhere().await;
            } else if *i > 10 {
                yield_once().await;
            }
            if *i % 2 != 0 {
                return Err(());
            }
            Ok(*i as i64)
        }
    }

    async fn start_widen(thing: Rc<Thing>, i: i32) -> (i32, Result<i64, ()>) {
        let widened = thing.widen(&i).await;
        (i, widened)
    }

    struct PolledThing<Q> {
        poller: LocalPollAdapter<Rc<Thing>, i32, Result<i64, ()>, Q>,
    }

    fn polled_thing() -> PolledThing<impl Future<Output = (i32, Result<i64, ()>)>> {
        PolledThing { poller: LocalPollAdapter::new(Rc::new(Thing {}), start_widen) }
    }

    impl<Q: Future<Output = (i32, Result<i64, ()>)>> PolledThing<Q> {
        fn widen(&mut self, i: &i32) -> Poll<Result<i64, ()>> {
            self.poller.poll(*i)
        }

        fn wait(&mut self) -> bool {
            self.poller.wait()
        }
    }

    #[test]
    fn immediate_success() {
        let mut p = polled_thing();
        assert_eq!(p.widen(&2), Poll::Ready(Ok(2)));
        assert!(p.wait());
    }

    #[test]
    fn immediate_error() {
        let mut p = polled_thing();
        assert_eq!(p.widen(&1), Poll::Ready(Err(())));
        assert!(p.wait());
    }

    #[test]
    fn deferred_error() {
        let mut p = polled_thing();
        assert_eq!(p.widen(&101), Poll::Pending);
        assert!(!p.wait());
        assert_eq!(p.widen(&101), Poll::Ready(Err(())));
        assert!(p.wait());
        // Errors are not kept.
        assert_eq!(p.widen(&101), Poll::Pending);
        assert!(!p.wait());
        assert_eq!(p.widen(&101), Poll::Ready(Err(())));
        assert!(p.wait());
    }

    #[test]
    fn deferred_success() {
        let mut p = polled_thing();
        assert_eq!(p.widen(&50), Poll::Pending);
        assert!(!p.wait());
        assert_eq!(p.widen(&50), Poll::Ready(Ok(50)));
        assert!(p.wait());
        // Success is kept.
        assert_eq!(p.widen(&50), Poll::Ready(Ok(50)));
        assert!(p.wait());
    }

    #[test]
    fn waits_for_another_thread() {
        let mut p = polled_thing();
        assert_eq!(p.widen(&2000), Poll::Pending);
        assert_eq!(p.widen(&50), Poll::Pending);
        assert_eq!(p.poller.pending_count(), 2);
        assert!(!p.wait());
        assert_eq!(p.widen(&2000), Poll::Ready(Ok(2000)));
        assert_eq!(p.widen(&50), Poll::Ready(Ok(50)));
    }
}
