//! Reaches the same standard library code another way: through vectors made of one element,
//! a map, a directory walked and a file read.
use std::{collections::HashMap, env, fs, path::Path};

fn walk(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() { walk(&path, found) } else { found.push(path) }
    }
}

fn main() {
    let mut sizes: HashMap<String, usize> = env::vars().map(|(name, value)| (name, value.len())).collect();
    let mut found = Vec::new();
    walk(&env::current_dir().unwrap(), &mut found);
    for path in &found {
        let last = path.components().next_back().map(|it| it.as_os_str().to_string_lossy().into_owned());
        let text = fs::read_to_string(path).unwrap_or_default();
        let zeros = vec![0u8; text.len() % 7];
        let ones = vec![1usize; text.lines().count() % 5];
        *sizes.entry(last.unwrap_or_default()).or_default() += zeros.len() + ones.iter().sum::<usize>();
    }
    let mut all: Vec<_> = sizes.into_iter().collect();
    all.sort();
    println!("{} {}", all.len(), std::backtrace::Backtrace::capture());
}
