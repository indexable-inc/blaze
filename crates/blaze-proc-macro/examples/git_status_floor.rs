//! The cost of recomputing a `git status` fact against its floor, on a real checkout.
//!
//! `git_status_floor <checkout> <folder>`: five rounds each, median printed, of
//! (1) the floor: one read of `.git/index` and one `lstat` per entry, in a plain loop;
//! (2) the floor with git's own leading-folder check: (1) plus one `lstat` per distinct folder;
//! (3) `answer_now` of `status --porcelain` and of `status --porcelain --untracked-files=no` in
//!     `<folder>`, as a repeat command recomputes them (D54).
//! Reads only; runs nothing.

use std::{path::PathBuf, time::{Duration, Instant}};

use blaze_build_script::git;
use blaze_own::Alloc;
use blaze_proc_macro::script_sandbox::{answer_now, Tree};

const ROUNDS: usize = 5;

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, checkout, folder] = &args[..] else {
        eprintln!("usage: git_status_floor <checkout> <folder>");
        std::process::exit(2);
    };
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here and handed down.
    let frame = blaze_mem::Frame::with_reserve(1 << 34, blaze_mem::Tag::Other).expect("the entry's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc: Alloc<'_> = erased.handle();
    let root = PathBuf::from(checkout);
    let mut floor = Vec::new();
    let mut with_folders = Vec::new();
    let mut entries = 0;
    let mut folders = 0;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        let bytes = std::fs::read(root.join(".git/index")).expect("the checkout's .git/index");
        let paths = git::index_paths(&bytes, alloc).expect("an index status can read");
        for path in paths.iter() {
            std::hint::black_box(std::fs::symlink_metadata(root.join(path.as_str())).is_ok());
        }
        floor.push(start.elapsed());
        entries = paths.len();

        let start = Instant::now();
        let bytes = std::fs::read(root.join(".git/index")).expect("the checkout's .git/index");
        let paths = git::index_paths(&bytes, alloc).expect("an index status can read");
        let mut seen: Vec<&str> = Vec::new();
        for path in paths.iter() {
            for (end, _) in path.match_indices('/') {
                let leading = &path.as_str()[..end];
                if let Err(at) = seen.binary_search(&leading) {
                    std::hint::black_box(std::fs::symlink_metadata(root.join(leading)).is_ok());
                    seen.insert(at, leading);
                }
            }
            std::hint::black_box(std::fs::symlink_metadata(root.join(path.as_str())).is_ok());
        }
        with_folders.push(start.elapsed());
        folders = seen.len();
    }
    println!("floor: one index read + {entries} lstat: median {:?} of {ROUNDS}", median(floor));
    println!("floor with git's leading-folder check: + {folders} folder lstat: median {:?} of {ROUNDS}", median(with_folders));
    // The served root, resolved once as the command resolves it (an example's argument may be
    // relative: made absolute here, the one place the process's folder is asked).
    let absolute = std::path::absolute(&root).expect("the checkout's absolute path");
    let tree = Tree::Dir(blaze_proc_macro::script_sandbox::RealRoot::resolve(&absolute).expect("the checkout resolves"));
    // tempo-node's questions, in the order its record lists them.
    let questions = ["--version", "rev-parse --is-inside-work-tree", "log -1 --pretty=format:%cI", "status --porcelain", "describe --always --tags --match tempo/v[0-9]*", "status --porcelain --untracked-files=no", "rev-parse HEAD"];
    for words in questions {
        let subject = format!("git:{folder}\n{words}");
        let mut times = Vec::new();
        let mut answer = None;
        for _ in 0..ROUNDS {
            let start = Instant::now();
            answer = Some(answer_now(&tree, &subject).expect("a git subject"));
            times.push(start.elapsed());
        }
        println!("answer_now `git {words}`: median {:?} of {ROUNDS}, answered: {}", median(times), answer.flatten().is_some());
    }
}
