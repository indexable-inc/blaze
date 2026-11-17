//! s5-thr-02: `mpsc` channels: many producers (received in an order the scheduler picks, so
//! sorted before printing), a rendezvous `sync_channel(0)`, `recv` after every sender is gone
//! (`Err(RecvError)`: "This function will always block the current thread if there is no data
//! available and it's possible for more data to be sent ... it will return Err" once all senders
//! dropped), `try_recv` (`Empty`, then `Disconnected`), `recv_timeout`, and a two-stage pipeline
//! (one producer per channel: FIFO, so its order is fixed).

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn main() {
    let (tx, rx) = mpsc::channel();
    for id in 0..3 {
        let tx = tx.clone();
        thread::spawn(move || {
            for k in 0..3 {
                tx.send(id * 10 + k).unwrap();
            }
        });
    }
    drop(tx);
    let mut got: Vec<i32> = rx.iter().collect();
    got.sort();
    println!("{:?}", got);
    let (tx, rx) = mpsc::sync_channel::<String>(0);
    let h = thread::spawn(move || {
        for w in ["ping", "pong"] {
            tx.send(w.to_string()).unwrap();
        }
    });
    println!("{} {}", rx.recv().unwrap(), rx.recv().unwrap());
    h.join().unwrap();
    println!("{:?}", rx.recv());
    let (tx, rx) = mpsc::channel::<i32>();
    println!("{:?}", rx.try_recv());
    tx.send(5).unwrap();
    println!("{:?}", rx.recv_timeout(Duration::from_secs(5)));
    drop(tx);
    println!("{:?}", rx.try_recv());
    let (a_tx, a_rx) = mpsc::channel::<i32>();
    let (b_tx, b_rx) = mpsc::channel::<i32>();
    let stage = thread::spawn(move || {
        for v in a_rx {
            b_tx.send(v * v).unwrap();
        }
    });
    for v in 1..=4 {
        a_tx.send(v).unwrap();
    }
    drop(a_tx);
    println!("{:?}", b_rx.iter().collect::<Vec<i32>>());
    stage.join().unwrap();
}
