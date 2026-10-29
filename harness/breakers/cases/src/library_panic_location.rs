//! tests/ui/iterators/iter-sum-overflow-debug.rs: a panic in code of the standard library
//! that is compiled into the crate, as `Iterator::sum` is, names the file where the sources
//! are installed. blaze named it `/rustc/<commit>/..`, which is where the library was built
//! and what only the code inside the built library says. Where they are installed is each
//! one's own (rustc's toolchain's `rust-src`, blaze's `~/.blaze/library`), so what is compared
//! is the file from `library/` on and that it is there.

use std::panic::catch_unwind;
use std::sync::{Arc, Mutex};

#[inline(never)]
fn sum(values: &[u8]) -> Option<u8> {
    catch_unwind(|| values.iter().copied().sum::<u8>()).ok()
}

pub fn run() -> (Option<u8>, Vec<(String, bool)>) {
    let hook = std::panic::take_hook();
    let files = Arc::new(Mutex::new(Vec::new()));
    let seen = files.clone();
    std::panic::set_hook(Box::new(move |info| {
        seen.lock().unwrap().extend(info.location().map(|at| at.file().to_owned()));
    }));
    let summed = sum(&[200, 100]);
    std::panic::set_hook(hook);
    let files = files.lock().unwrap().iter().map(|file| {
        let file = file.replace('\\', "/");
        let named = file.rfind("/library/").map_or(file.as_str(), |at| &file[at + 1..]).to_owned();
        (named, std::path::Path::new(&file).is_file())
    }).collect();
    (summed, files)
}
