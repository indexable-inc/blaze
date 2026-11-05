use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;

static HITS: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static EVENTS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static CONFIG: OnceLock<String> = OnceLock::new();
static TABLE: LazyLock<BTreeMap<u32, &'static str>> =
    LazyLock::new(|| BTreeMap::from([(1, "one"), (2, "two"), (3, "three")]));

fn record(n: u32) {
    HITS.fetch_add(1, Ordering::SeqCst);
    TOTAL.fetch_add(u64::from(n), Ordering::SeqCst);
    EVENTS.lock().unwrap().push(n);
}

fn config() -> &'static str {
    CONFIG.get_or_init(|| format!("config-{}", TABLE.len()))
}

fn main() {
    let handles: Vec<_> = (1..=8)
        .map(|n| {
            thread::spawn(move || {
                record(n);
                config().len()
            })
        })
        .collect();
    let lens: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let mut events = EVENTS.lock().unwrap().clone();
    events.sort();
    println!("hits {}", HITS.load(Ordering::SeqCst));
    println!("total {}", TOTAL.load(Ordering::SeqCst));
    println!("events {events:?}");
    println!("config {} {lens:?}", config());
    println!("table {:?}", *TABLE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records() {
        let before = HITS.load(Ordering::SeqCst);
        record(3);
        assert!(HITS.load(Ordering::SeqCst) > before);
        assert!(EVENTS.lock().unwrap().contains(&3));
    }

    #[test]
    fn lazy() {
        assert_eq!(config(), "config-3");
        assert_eq!(TABLE[&2], "two");
    }
}
