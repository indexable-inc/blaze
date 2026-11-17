//! The channel's tests. Between real threads: every value arrives once, in each sender's
//! order, and a disconnect ends every waiter. Under `RUSTFLAGS="--cfg blaze_check" cargo test
//! -p blaze-core --release model_chan`: the eventcount and the disconnect under every
//! interleaving and every value C11 lets a load read, each with its planted bug ([`Plant`]).

#[cfg(not(blaze_check))]
mod threads {
    use std::{thread, time::Duration};

    use super::super::{
        RecvTimeoutError, SendTimeoutError, TryRecvError, TrySendError, bounded, never, unbounded,
    };

    const SENDERS: usize = 4;
    const EACH: usize = 10_000;

    fn spawn<R: Send + 'static>(
        name: &str,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> thread::JoinHandle<R> {
        thread::Builder::new().name(name.to_owned()).spawn(work).expect("a test thread")
    }

    /// `SENDERS` senders and two receivers: each value arrives once, each sender's in order.
    fn arrives_once(cap: Option<usize>) {
        let (send, recv) = match cap {
            Some(cap) => bounded(cap),
            None => unbounded(),
        };
        let senders: Vec<_> = (0..SENDERS)
            .map(|me| {
                let send = send.clone();
                spawn("chan test sender", move || {
                    for at in 0..EACH {
                        send.send(me * EACH + at).expect("a receiver is there");
                    }
                })
            })
            .collect();
        drop(send);
        let receivers: Vec<_> = (0..2)
            .map(|_| {
                let recv = recv.clone();
                spawn("chan test receiver", move || {
                    let mut last = [None::<usize>; SENDERS];
                    let mut got = Vec::new();
                    for value in &recv {
                        let from = value / EACH;
                        assert!(last[from] < Some(value), "a sender's values out of order");
                        last[from] = Some(value);
                        got.push(value);
                    }
                    got
                })
            })
            .collect();
        drop(recv);
        for sender in senders {
            sender.join().expect("a sender ends");
        }
        let mut all: Vec<usize> =
            receivers.into_iter().flat_map(|it| it.join().expect("a receiver ends")).collect();
        all.sort_unstable();
        assert!(all.iter().copied().eq(0..SENDERS * EACH), "a value lost or received twice");
    }

    #[test]
    fn chan_bounded_hands_every_value_over_once() {
        arrives_once(Some(8));
    }

    #[test]
    fn chan_bounded_by_one_hands_every_value_over_once() {
        arrives_once(Some(1));
    }

    #[test]
    fn chan_unbounded_hands_every_value_over_once() {
        arrives_once(None);
    }

    #[test]
    fn chan_rendezvous_hands_every_value_over_once() {
        arrives_once(Some(0));
    }

    /// A full ring refuses a try and times a send out; a gone receiver refuses with the value.
    #[test]
    fn chan_full_and_disconnected_sends_give_the_value_back() {
        let (send, recv) = bounded(1);
        send.send(1).expect("room");
        assert_eq!(send.try_send(2), Err(TrySendError::Full(2)));
        assert_eq!(
            send.send_timeout(3, Duration::from_millis(10)),
            Err(SendTimeoutError::Timeout(3))
        );
        drop(recv);
        assert_eq!(send.send(4).map_err(|it| it.0), Err(4));
    }

    /// A receiver waiting on an empty channel wakes when its last sender drops, after the
    /// values sent before it.
    #[test]
    fn chan_last_sender_drop_wakes_a_waiting_receiver() {
        for cap in [Some(0), Some(4), None] {
            let (send, recv) = match cap {
                Some(cap) => bounded(cap),
                None => unbounded(),
            };
            let waiter = spawn("chan test waiter", move || {
                let first = recv.recv();
                (first, recv.recv())
            });
            send.send(7).expect("a receiver");
            drop(send);
            let (first, second) = waiter.join().expect("the receiver ends");
            assert_eq!(first, Ok(7));
            assert!(second.is_err(), "disconnected after the last value");
        }
    }

    /// `never` times out and is never disconnected; an empty channel's try says empty.
    #[test]
    fn chan_never_times_out_and_try_says_empty() {
        let recv = never::<u32>();
        assert_eq!(recv.recv_timeout(Duration::from_millis(5)), Err(RecvTimeoutError::Timeout));
        let (_send, recv) = unbounded::<u32>();
        assert_eq!(recv.try_recv(), Err(TryRecvError::Empty));
    }

    /// A named timed receive takes what is there, times out on an empty channel, and is
    /// disconnected once the senders are gone, for every flavor. Prints nothing.
    #[test]
    fn chan_named_timed_receive_is_the_timed_receive() {
        for cap in [Some(0), Some(1), None] {
            let (send, recv) = match cap {
                Some(cap) => bounded::<u32>(cap),
                None => unbounded::<u32>(),
            };
            let what = "the test's named value";
            assert_eq!(recv.recv_timeout_named(what, Duration::from_millis(5)), Err(RecvTimeoutError::Timeout));
            if cap != Some(0) {
                send.send(3).expect("room");
                assert_eq!(recv.recv_timeout_named(what, Duration::from_secs(5)), Ok(3));
            }
            drop(send);
            assert_eq!(recv.recv_timeout_named(what, Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected));
        }
    }

    /// A rendezvous send with no receiver times out with its value back.
    #[test]
    fn chan_rendezvous_send_times_out_untaken() {
        let (send, _recv) = bounded::<u32>(0);
        assert_eq!(
            send.send_timeout(5, Duration::from_millis(5)),
            Err(SendTimeoutError::Timeout(5))
        );
        assert_eq!(send.try_send(6), Err(TrySendError::Full(6)));
    }
}

#[cfg(blaze_check)]
mod checked {
    use blaze_check::{model, thread};

    use super::super::{Plant, bounded, unbounded};

    /// A sender sends one value while a receiver waits for it: it arrives. With
    /// [`Plant::Unfenced`] the sender may read the waiters' count from before the receiver
    /// counted itself, while the receiver's look missed the push: it sleeps forever.
    fn a_waiter_hears_the_send(plant: Plant) {
        model(move || {
            let (send, recv) = unbounded::<u32>();
            let sender = thread::spawn(move || {
                send.send_with(7, plant).expect("a receiver");
                // Kept alive so that only the notify, never the disconnect, can wake it.
                send
            });
            assert_eq!(recv.recv(), Ok(7));
            drop(sender.join().unwrap());
        });
    }

    #[test]
    fn model_chan_a_waiting_receiver_hears_the_send() {
        a_waiter_hears_the_send(Plant::None);
    }

    /// The control: without the fence the notify is lost.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn model_chan_unfenced_notify_is_lost() {
        a_waiter_hears_the_send(Plant::Unfenced);
    }

    /// A sender puts two values into a ring of one slot while the model's thread receives:
    /// the second send waits for room and is woken by the first receive; both arrive, in
    /// order. (The receiver is not also a sender: a thread whose own send waits on a full ring
    /// it alone drains is a deadlock of the test, which the model rightly reports.)
    #[test]
    fn model_chan_a_full_ring_wakes_its_sender() {
        model(|| {
            let (send, recv) = bounded::<u32>(1);
            let sender = thread::spawn(move || {
                send.send(1).expect("a receiver");
                send.send(2).expect("a receiver");
            });
            assert_eq!([recv.recv().unwrap(), recv.recv().unwrap()], [1, 2]);
            sender.join().unwrap();
        });
    }

    /// A receiver waits on an empty channel whose one sender drops: it is told. With
    /// [`Plant::SilentDisconnect`] the drop wakes no one and it sleeps forever.
    fn a_waiter_hears_the_disconnect(plant: Plant) {
        model(move || {
            let (send, recv) = bounded::<u32>(1);
            let sender = thread::spawn(move || send.drop_planted(plant));
            assert!(recv.recv().is_err(), "disconnected");
            sender.join().unwrap();
        });
    }

    #[test]
    fn model_chan_a_waiting_receiver_hears_the_disconnect() {
        a_waiter_hears_the_disconnect(Plant::None);
    }

    /// The control: a silent disconnect leaves the receiver asleep.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn model_chan_silent_disconnect_is_lost() {
        a_waiter_hears_the_disconnect(Plant::SilentDisconnect);
    }

    /// A rendezvous: the send returns only once the receive took its value.
    #[test]
    fn model_chan_rendezvous_send_waits_for_its_take() {
        model(|| {
            let (send, recv) = bounded::<u32>(0);
            let receiver = thread::spawn(move || recv.recv());
            send.send(3).expect("a receiver");
            assert_eq!(receiver.join().unwrap(), Ok(3));
        });
    }
}
