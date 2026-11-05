use std::sync::mpsc;
use std::thread;

fn sum_in_threads(n: u64, threads: u64) -> u64 {
    let handles: Vec<_> = (0..threads)
        .map(|t| thread::spawn(move || (0..n).filter(|i| i % threads == t).sum::<u64>()))
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).sum()
}

fn doubled(mut data: Vec<u32>) -> Vec<u32> {
    thread::scope(|s| {
        for x in data.iter_mut() {
            s.spawn(move || *x *= 2);
        }
    });
    data
}

fn main() {
    println!("sum {}", sum_in_threads(10_000, 8));

    let named = thread::Builder::new()
        .name("worker".into())
        .spawn(|| thread::current().name().unwrap().to_string())
        .unwrap();
    println!("name {}", named.join().unwrap());

    let (tx, rx) = mpsc::channel();
    for i in 0..4u32 {
        let tx = tx.clone();
        thread::spawn(move || tx.send(i * 10).unwrap());
    }
    drop(tx);
    let mut got: Vec<u32> = rx.iter().collect();
    got.sort();
    println!("channel {got:?}");

    println!("scoped {:?}", doubled(vec![1, 2, 3, 4]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums() {
        assert_eq!(sum_in_threads(100, 3), 4950);
    }

    #[test]
    fn scoped() {
        assert_eq!(doubled(vec![5, 6]), vec![10, 12]);
    }
}
