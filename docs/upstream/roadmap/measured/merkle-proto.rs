//! Prototype to settle numbers for docs/roadmap.md: what a content-addressed Merkle VFS costs
//! per write on a real checkout (nushell), BLAKE3 against git's SHA-1 object hashing.
use std::{collections::BTreeMap, path::Path, process::Command, sync::Arc, time::Instant};

type Hash = [u8; 32];

enum Node {
    File(Hash, Arc<[u8]>),
    Dir(BTreeMap<String, Arc<Node>>, Hash),
}

impl Node {
    fn hash(&self) -> Hash {
        match self {
            Node::File(h, _) | Node::Dir(_, h) => *h,
        }
    }
}

fn dir_hash(entries: &BTreeMap<String, Arc<Node>>) -> Hash {
    let mut h = blake3::Hasher::new();
    for (name, node) in entries {
        h.update(&[matches!(**node, Node::Dir(..)) as u8]);
        h.update(name.as_bytes());
        h.update(&[0]);
        h.update(&node.hash());
    }
    *h.finalize().as_bytes()
}

fn file(bytes: Arc<[u8]>) -> Arc<Node> {
    Arc::new(Node::File(*blake3::hash(&bytes).as_bytes(), bytes))
}

fn build(files: &[(Vec<String>, Arc<[u8]>)]) -> Arc<Node> {
    let mut tree: BTreeMap<String, Vec<(Vec<String>, Arc<[u8]>)>> = BTreeMap::new();
    let mut leaves = BTreeMap::new();
    for (path, bytes) in files {
        if path.len() == 1 {
            leaves.insert(path[0].clone(), file(bytes.clone()));
        } else {
            tree.entry(path[0].clone()).or_default().push((path[1..].to_vec(), bytes.clone()));
        }
    }
    for (name, sub) in tree {
        leaves.insert(name, build(&sub));
    }
    let h = dir_hash(&leaves);
    Arc::new(Node::Dir(leaves, h))
}

/// Path-copying write: a new root sharing every untouched subtree.
fn write(root: &Arc<Node>, path: &[&str], bytes: Arc<[u8]>) -> Arc<Node> {
    let Node::Dir(entries, _) = &**root else { unreachable!() };
    let mut entries = entries.clone();
    let child = if path.len() == 1 {
        file(bytes)
    } else {
        write(&entries[path[0]], &path[1..], bytes)
    };
    entries.insert(path[0].to_owned(), child);
    let h = dir_hash(&entries);
    Arc::new(Node::Dir(entries, h))
}

/// Paths that differ between two roots, skipping equal subtrees by hash.
fn diff(a: &Arc<Node>, b: &Arc<Node>, at: &mut Vec<String>, out: &mut Vec<String>, visited: &mut usize) {
    *visited += 1;
    if a.hash() == b.hash() {
        return;
    }
    match (&**a, &**b) {
        (Node::Dir(x, _), Node::Dir(y, _)) => {
            for (name, nx) in x {
                at.push(name.clone());
                match y.get(name) {
                    Some(ny) => diff(nx, ny, at, out, visited),
                    None => out.push(at.join("/")),
                }
                at.pop();
            }
            for name in y.keys().filter(|n| !x.contains_key(*n)) {
                at.push(name.clone());
                out.push(at.join("/"));
                at.pop();
            }
        }
        _ => out.push(at.join("/")),
    }
}

fn git_blob_sha1(bytes: &[u8]) -> [u8; 20] {
    let mut h = sha1_smol::Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    h.digest().bytes()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let repo = std::env::args().nth(1).expect("repo");
    let out = Command::new("git").args(["-C", &repo, "ls-files"]).output().unwrap();
    let names: Vec<String> = String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect();
    let files: Vec<(Vec<String>, Arc<[u8]>)> = names
        .iter()
        .filter_map(|n| {
            let bytes = std::fs::read(Path::new(&repo).join(n)).ok()?;
            Some((n.split('/').map(str::to_owned).collect(), Arc::from(bytes)))
        })
        .collect();
    let mut t = vec![];
    for _ in 0..21 {
        let s = Instant::now();
        for n in &names {
            std::hint::black_box(std::fs::metadata(Path::new(&repo).join(n)).ok());
        }
        t.push(s.elapsed().as_secs_f64() * 1e3);
    }
    println!("stat of every file (one polling pass), page cache warm: {:.2} ms", median(t));
    let mut t = vec![];
    for _ in 0..5 {
        let s = Instant::now();
        for n in &names {
            std::hint::black_box(std::fs::read(Path::new(&repo).join(n)).ok());
        }
        t.push(s.elapsed().as_secs_f64() * 1e3);
    }
    println!("read of every file, page cache warm, one thread: {:.2} ms", median(t));
    let total: usize = files.iter().map(|f| f.1.len()).sum();
    let max_depth = files.iter().map(|f| f.0.len()).max().unwrap();
    println!("files {} bytes {} max depth {}", files.len(), total, max_depth);

    let mut t = vec![];
    let mut root = build(&files);
    for _ in 0..5 {
        let s = Instant::now();
        root = build(&files);
        t.push(s.elapsed().as_secs_f64() * 1e3);
    }
    println!("whole tree, blake3, one thread: {:.2} ms", median(t));
    let mut t = vec![];
    for _ in 0..5 {
        let s = Instant::now();
        for f in &files {
            std::hint::black_box(git_blob_sha1(&f.1));
        }
        t.push(s.elapsed().as_secs_f64() * 1e3);
    }
    println!("whole tree, git blob sha1 (blobs only), one thread: {:.2} ms", median(t));

    // A typical agent transaction: three files, one of them nushell's 25 KB main.rs.
    let picks = ["src/main.rs", "crates/nu-protocol/src/value/mod.rs", "crates/nu-command/src/filters/each.rs"];
    let picks: Vec<&str> = picks.into_iter().filter(|p| names.iter().any(|n| n == p)).collect();
    let edits: Vec<(Vec<&str>, Arc<[u8]>)> = picks
        .iter()
        .map(|p| {
            let mut b = std::fs::read(Path::new(&repo).join(p)).unwrap();
            b.extend_from_slice(b"\n// edited\n");
            (p.split('/').collect(), Arc::from(b))
        })
        .collect();
    let edit_bytes: usize = edits.iter().map(|e| e.1.len()).sum();
    let mut t = vec![];
    let mut new = root.clone();
    for _ in 0..2001 {
        let s = Instant::now();
        new = root.clone();
        for (p, b) in &edits {
            new = write(&new, p, b.clone());
        }
        t.push(s.elapsed().as_secs_f64() * 1e6);
    }
    println!("transaction of {} files, {} bytes: {:.1} us (median of 2001)", edits.len(), edit_bytes, median(t));
    let mut t = vec![];
    for _ in 0..2001 {
        let s = Instant::now();
        for (_, b) in &edits {
            std::hint::black_box(blake3::hash(b));
        }
        t.push(s.elapsed().as_secs_f64() * 1e6);
    }
    println!("  of which hashing the file bytes: {:.1} us", median(t));
    let mut t = vec![];
    let mut changed = vec![];
    let mut visited = 0;
    for _ in 0..2001 {
        changed.clear();
        visited = 0;
        let s = Instant::now();
        diff(&root, &new, &mut vec![], &mut changed, &mut visited);
        t.push(s.elapsed().as_secs_f64() * 1e6);
    }
    println!("diff of the two roots: {:.1} us, {} nodes visited, changed {:?}", median(t), visited, changed);
    let gib = |bytes: usize, ms: f64| bytes as f64 / (ms / 1e3) / (1u64 << 30) as f64;
    let big: Vec<u8> = (0..64 << 20).map(|i| (i * 31) as u8).collect();
    let s = Instant::now();
    std::hint::black_box(blake3::hash(&big));
    let ms = s.elapsed().as_secs_f64() * 1e3;
    println!("blake3 64 MiB one thread: {:.1} GiB/s", gib(big.len(), ms));
}
