//! s9-app-02: three worker threads take lines from a shared `Mutex<Vec<_>>` queue, count words in
//! a `HashMap` each, and send them over a channel; the main thread merges into a `BTreeMap`.
//! Which worker takes which line is the scheduler's choice, so only order-free results are
//! printed: per-line totals sorted by line number, the number of jobs (4), the top words by
//! count then name, and the totals.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

const TEXT: &str = "the quick brown fox\njumps over the lazy dog\nthe dog barks\nquick quick fox";

fn main() {
    let lines: Vec<(usize, String)> = TEXT.lines().map(String::from).enumerate().collect();
    let queue = Arc::new(Mutex::new(lines));
    let (tx, rx) = mpsc::channel::<(usize, HashMap<String, usize>)>();
    let workers: Vec<thread::JoinHandle<usize>> = (0..3)
        .map(|_| {
            let q = Arc::clone(&queue);
            let tx = tx.clone();
            thread::spawn(move || {
                let mut done = 0;
                loop {
                    let job = q.lock().unwrap().pop();
                    let Some((no, line)) = job else { break };
                    let mut m = HashMap::new();
                    for w in line.split_whitespace() {
                        *m.entry(w.to_string()).or_insert(0) += 1;
                    }
                    tx.send((no, m)).unwrap();
                    done += 1;
                }
                done
            })
        })
        .collect();
    drop(tx);
    let mut per_line: Vec<(usize, usize)> = Vec::new();
    let mut total: BTreeMap<String, usize> = BTreeMap::new();
    for (no, m) in rx {
        per_line.push((no, m.values().sum()));
        for (w, c) in m {
            *total.entry(w).or_default() += c;
        }
    }
    let jobs: usize = workers.into_iter().map(|h| h.join().unwrap()).sum();
    per_line.sort();
    println!("{:?}", per_line);
    println!("jobs {jobs}");
    let mut top: Vec<(&String, &usize)> = total.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (w, c) in top.iter().take(3) {
        println!("{w} {c}");
    }
    println!("{} distinct, {} words", total.len(), total.values().sum::<usize>());
}
