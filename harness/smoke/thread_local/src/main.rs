use std::cell::{Cell, RefCell};
use std::thread;

thread_local! {
    static COUNTER: Cell<u32> = const { Cell::new(0) };
    static LOG: RefCell<Vec<String>> = RefCell::new(Vec::new());
}

fn bump(by: u32) -> u32 {
    COUNTER.with(|c| {
        c.set(c.get() + by);
        c.get()
    })
}

fn main() {
    bump(1);
    bump(2);
    LOG.with_borrow_mut(|log| log.push("main".to_string()));
    let others: Vec<u32> = (1..=3)
        .map(|i| {
            thread::spawn(move || {
                bump(i);
                bump(i)
            })
            .join()
            .unwrap()
        })
        .collect();
    println!("main {}", COUNTER.get());
    println!("others {others:?}");
    LOG.with_borrow(|log| println!("log {log:?}"));
    let fresh = thread::spawn(|| LOG.with_borrow(|log| log.len())).join().unwrap();
    println!("fresh log {fresh}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_counter() {
        assert_eq!(thread::spawn(|| bump(5)).join().unwrap(), 5);
    }

    #[test]
    fn own_log() {
        LOG.with_borrow_mut(|log| log.push("test".to_string()));
        assert_eq!(LOG.with_borrow(|log| log.len()), 1);
    }
}
