//! The VFS on a real checkout: `cargo run --release -p blaze-vfs --example vfs_bench -- <repo>`.
//! Loads the files git tracks into one root, then times a three-file transaction, the diff of
//! the two roots, journaling the root and recovering it.
#![allow(clippy::print_stdout)]

use std::{process::Command, sync::Arc, time::Instant};

use blaze_core::FileName;
use blaze_vfs::{Op, Vfs, journal::Journal};

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut samples = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        let start = Instant::now();
        last = Some(f());
        samples.push(start.elapsed().as_secs_f64() * 1e6);
    }
    (median(samples), last.expect("a run"))
}

fn main() {
    let repo = std::env::args().nth(1).expect("a checkout");
    #[allow(clippy::disallowed_methods)]
    let out =
        Command::new("git").current_dir(&repo).args(["ls-files", "-z"]).output().expect("git");
    let files: Vec<(String, Arc<[u8]>)> = out
        .stdout
        .split(|it| *it == 0)
        .filter(|it| !it.is_empty())
        .filter_map(|path| {
            let path = String::from_utf8(path.to_vec()).ok()?;
            let bytes = std::fs::read(format!("{repo}/{path}")).ok()?;
            Some((path, Arc::from(bytes)))
        })
        .collect();
    let bytes: usize = files.iter().map(|it| it.1.len()).sum();
    println!("files {} bytes {bytes}", files.len());
    let writes: Vec<Op> = files
        .iter()
        .map(|(path, bytes)| Op::Write { path: path.clone(), bytes: bytes.clone() })
        .collect();

    let (us, mut vfs) = time(9, || {
        let mut vfs = Vfs::new();
        let empty = vfs.tree.hash(vfs.tree.empty());
        vfs.commit("w", empty, &writes).expect("a tree");
        vfs
    });
    println!("whole tree into an empty VFS, one transaction: {:.2} ms (median of 9)", us / 1e3);
    let base = vfs.head("w");

    // The three files the prototype edited, or the three largest Rust files elsewhere.
    let mut picked: Vec<&(String, Arc<[u8]>)> = [
        "crates/nu-command/src/filters/each.rs",
        "crates/nu-protocol/src/value/mod.rs",
        "src/main.rs",
    ]
    .iter()
    .filter_map(|want| files.iter().find(|it| it.0 == *want))
    .collect();
    if picked.len() < 3 {
        picked = files.iter().filter(|it| it.0.ends_with(".rs")).collect();
        picked.sort_by_key(|it| std::cmp::Reverse(it.1.len()));
        picked.truncate(3);
    }
    let edits: Vec<Op> = picked
        .iter()
        .map(|(path, bytes)| Op::Edit {
            path: path.clone(),
            at: bytes.len() as u64 / 2,
            len: 0,
            bytes: b"// edited\n".to_vec(),
        })
        .collect();
    let edited: usize = picked.iter().map(|it| it.1.len()).sum();
    let (us, next) = time(2001, || vfs.tree.apply(base, &edits).expect("edits"));
    println!("transaction of {} files, {edited} bytes: {us:.1} us (median of 2001)", edits.len());
    let (us, changes) = time(2001, || vfs.tree.diff(base, next));
    println!("diff of the two roots: {us:.1} us, {} changed", changes.len());

    let file = FileName::new("journal").unwrap();
    let base_hash = vfs.tree.hash(base);
    let mut journal = Journal::new(Vfs::new(), file.clone(), 0);
    let empty = journal.vfs().tree.hash(journal.vfs().tree.empty());
    journal.commit("w", empty, &writes).expect("a tree");
    let mut effects = Vec::new();
    let (us, _) = time(1, || journal.flush(&mut effects));
    let Some(blaze_core::Effect::Append { bytes: log, .. }) = effects.pop() else {
        panic!("an append")
    };
    println!("journal of the whole tree: {} bytes, framed in {:.2} ms", log.len(), us / 1e3);
    let (us, (recovered, found)) = time(9, || Journal::recover(file.clone(), 0, &log));
    assert_eq!(recovered.vfs().tree.hash(recovered.vfs().head("w")), base_hash);
    println!(
        "recovered: {} records, {:.2} ms (median of 9), root matches",
        found.records,
        us / 1e3
    );

    // Commits after recovery append only the nodes they make.
    let mut journal = recovered;
    let (us, _) = time(1, || {
        let current = journal.vfs().head("w");
        journal.commit("w", journal.vfs().tree.hash(current), &edits).expect("edits")
    });
    journal.flush(&mut effects);
    let Some(blaze_core::Effect::Append { bytes: log, .. }) = effects.pop() else {
        panic!("an append")
    };
    println!(
        "journaled three-file commit: {:.1} us, {} bytes appended (new nodes only)",
        us,
        log.len()
    );

    let size = vfs.tree.len();
    let (us, _) = time(1, || vfs.collect(&mut Vec::new()));
    println!("collect: {size} nodes to {} in {:.2} ms", vfs.tree.len(), us / 1e3);
}
