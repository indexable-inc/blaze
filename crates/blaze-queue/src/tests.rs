//! The queues' tests. Between real threads: that every value pushed is popped once, in each
//! producer's order. Under `RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-queue --release
//! model_`: every interleaving to the model's preemption bound and every value C11 lets a load
//! read, each slot's value a tracked place, and each queue's planted bug ([`QueuePlant`], through
//! the `Planted` seam) caught. Every queue is in `blaze_own::testing::heap()`'s shared handle,
//! never the global heap.
//!
//! [`QueuePlant`]: crate::QueuePlant

#[cfg(not(blaze_check))]
mod threads {
    use std::{sync::Arc, thread, vec::Vec};

    use blaze_own::testing::heap;

    use crate::{ArrayQueue, SegQueue};

    const PRODUCERS: usize = 4;
    const CONSUMERS: usize = 4;
    const EACH: usize = 20_000;

    fn spawn<R: Send + 'static>(
        name: &str,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> thread::JoinHandle<R> {
        thread::Builder::new().name(name.into()).spawn(work).expect("a test thread")
    }

    /// Every value of every producer is popped once, and each consumer sees each producer's
    /// values in the order it pushed them.
    fn hands_over_once(
        push: impl Fn(usize) + Send + Sync + 'static,
        pop: impl Fn() -> Option<usize> + Send + Sync + 'static,
    ) {
        let push = Arc::new(push);
        let pop = Arc::new(pop);
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|me| {
                let push = Arc::clone(&push);
                spawn("queue test producer", move || {
                    for at in 0..EACH {
                        push(me * EACH + at);
                    }
                })
            })
            .collect();
        let consumers: Vec<_> = (0..CONSUMERS)
            .map(|_| {
                let pop = Arc::clone(&pop);
                spawn("queue test consumer", move || {
                    let mut last = [None::<usize>; PRODUCERS];
                    let mut got = Vec::new();
                    while got.len() < PRODUCERS * EACH / CONSUMERS {
                        let Some(value) = pop() else { continue };
                        let from = value / EACH;
                        assert!(last[from] < Some(value), "a producer's values out of order");
                        last[from] = Some(value);
                        got.push(value);
                    }
                    got
                })
            })
            .collect();
        for producer in producers {
            producer.join().expect("a producer ends");
        }
        let mut all: Vec<usize> =
            consumers.into_iter().flat_map(|it| it.join().expect("a consumer ends")).collect();
        all.sort_unstable();
        assert!(all.iter().copied().eq(0..PRODUCERS * EACH), "a value lost or popped twice");
        assert!(pop().is_none());
    }

    #[test]
    fn array_queue_hands_every_value_over_once() {
        let queue = Arc::new(ArrayQueue::new_in(64, heap().shared()));
        let theirs = Arc::clone(&queue);
        hands_over_once(
            move |value| {
                let mut value = value;
                while let Err(back) = queue.push(value) {
                    value = back;
                }
            },
            move || theirs.pop(),
        );
    }

    #[test]
    fn seg_queue_hands_every_value_over_once() {
        let queue = Arc::new(SegQueue::<usize, _>::new_in(heap().shared()));
        let theirs = Arc::clone(&queue);
        hands_over_once(move |value| queue.push(value), move || theirs.pop());
    }

    /// A full ring hands the value back, its length counts what it holds, and a drop drops
    /// what is left.
    #[test]
    fn array_queue_refuses_when_full_and_drops_what_is_left() {
        let queue = ArrayQueue::new_in(3, heap().shared());
        for at in 0..3 {
            assert_eq!(queue.push(Arc::new(at)), Ok(()));
        }
        let kept = Arc::new(9);
        assert_eq!(queue.push(Arc::clone(&kept)).map_err(|it| *it), Err(9));
        assert!(queue.is_full());
        assert_eq!(queue.len(), 3);
        let first = queue.pop().expect("a value");
        assert_eq!(*first, 0);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.push(Arc::clone(&kept)), Ok(()));
        drop(queue);
        assert_eq!(Arc::strong_count(&kept), 1, "the drop dropped the value left in it");
    }

    /// Its length across blocks, and a drop drops what is left in every block.
    #[test]
    fn seg_queue_counts_across_blocks_and_drops_what_is_left() {
        let queue = SegQueue::<Arc<u32>, _, 3>::new_in(heap().shared());
        let kept = Arc::new(0);
        for _ in 0..10 {
            queue.push(Arc::clone(&kept));
        }
        assert_eq!(queue.len(), 10);
        for left in (6..10).rev() {
            assert!(queue.pop().is_some());
            assert_eq!(queue.len(), left);
        }
        drop(queue);
        assert_eq!(Arc::strong_count(&kept), 1, "the drop dropped every value left");
    }
}

#[cfg(blaze_check)]
mod checked {
    use blaze_check::{model, sync::Arc, thread};
    use std::vec::Vec;

    use blaze_own::testing::heap;

    use crate::{ArrayQueue, QueuePlant, SegQueue};

    /// Two producers push one value each into a ring of one slot while a consumer pops both:
    /// both arrive, once each. With [`QueuePlant::RelaxedPublish`] the consumer's read of a
    /// value races with its write.
    fn ring_hands_over(plant: QueuePlant) {
        model(move || {
            let queue = Arc::new(ArrayQueue::new_in(1, heap().shared()));
            let producers: Vec<_> = (1..3u32)
                .map(|value| {
                    let queue = queue.clone();
                    thread::spawn(move || {
                        let mut value = value;
                        while let Err(back) = queue.push_with(value, plant) {
                            value = back;
                            thread::yield_now();
                        }
                    })
                })
                .collect();
            let mut got = 0;
            let mut seen = 0u32;
            while got < 2 {
                match queue.pop() {
                    Some(value) => {
                        assert_eq!(seen & (1 << value), 0, "a value popped twice");
                        seen |= 1 << value;
                        got += 1;
                    }
                    None => thread::yield_now(),
                }
            }
            for producer in producers {
                producer.join().unwrap();
            }
            assert!(queue.pop().is_none());
        });
    }

    #[test]
    fn model_array_queue_hands_each_value_over_once() {
        ring_hands_over(QueuePlant::None);
    }

    /// The control: a relaxed publish of the slot lets the pop read the value unordered.
    #[test]
    #[should_panic(expected = "data race")]
    fn model_array_queue_relaxed_publish_races() {
        ring_hands_over(QueuePlant::RelaxedPublish);
    }

    /// One producer fills a block of two and starts the next while two consumers pop: each
    /// value arrives once, and the full block is freed by its last reader only. With
    /// [`QueuePlant::FreeUnasked`] the pop of its last slot frees it under the other reader.
    fn segments_free_after_the_last_read(plant: QueuePlant) {
        model(move || {
            let queue = Arc::new(SegQueue::<u32, _, 2>::new_in(heap().shared()));
            for value in 0..3 {
                queue.push(value);
            }
            let other = {
                let queue = queue.clone();
                thread::spawn(move || queue.pop_with(plant))
            };
            let mine = queue.pop_with(plant);
            let theirs = other.join().unwrap();
            let mut got = [mine, theirs, queue.pop_with(plant)];
            got.sort_unstable();
            assert_eq!(got, [Some(0), Some(1), Some(2)], "a value lost or popped twice");
            assert!(queue.pop().is_none());
        });
    }

    #[test]
    fn model_seg_queue_frees_a_block_after_its_last_read() {
        segments_free_after_the_last_read(QueuePlant::None);
    }

    /// The control: a block freed without asking its other readers is read after its free.
    #[test]
    #[should_panic(expected = "free")]
    fn model_seg_queue_free_unasked_reads_freed_memory() {
        segments_free_after_the_last_read(QueuePlant::FreeUnasked);
    }

    /// Two producers push across a block's end while one consumer pops: the block switch
    /// (installing the next block, moving the head to it) loses and repeats nothing.
    #[test]
    fn model_seg_queue_pushes_across_a_block_end() {
        model(|| {
            let queue = Arc::new(SegQueue::<u32, _, 2>::new_in(heap().shared()));
            queue.push(0);
            let producers: Vec<_> = (1..3u32)
                .map(|value| {
                    let queue = queue.clone();
                    thread::spawn(move || queue.push(value))
                })
                .collect();
            let mut seen = 0u32;
            let mut got = 0;
            while got < 3 {
                match queue.pop() {
                    Some(value) => {
                        assert_eq!(seen & (1 << value), 0, "a value popped twice");
                        seen |= 1 << value;
                        got += 1;
                    }
                    None => thread::yield_now(),
                }
            }
            for producer in producers {
                producer.join().unwrap();
            }
            assert_eq!(queue.len(), 0);
        });
    }
}
