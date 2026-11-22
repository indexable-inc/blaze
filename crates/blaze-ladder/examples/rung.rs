//! Runs one rung (or support program) in this process, as tests/ladder.rs does, and prints its
//! status, or the first refusal of the stage that stopped it, with every finding. The quick
//! probe of a rung that is not running yet: `examplerun.sh blaze-ladder rung 7b-dyn`. A hosted
//! program needs a host table and is refused here (`RunError::NoHost`); tests/hosted.rs runs it.

#[path = "../tests/rungs/mod.rs"]
mod rungs;

use blaze_ladder::refusal::SourceFile;

fn main() {
    let heap = blaze_own::Heap::process();
    let Some(rung) = std::env::args().nth(1) else {
        eprintln!("usage: rung <directory under crates/blaze-cli/tests/ladder>");
        std::process::exit(2);
    };
    let dir = rungs::ladder().join(&rung);
    let files = rungs::sources(&dir);
    let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
    match blaze_ladder::run::run(&views, "src/main.rs", None, rungs::x::overflow_of(&dir), heap.shared()) {
        Ok(status) => println!("{rung}: status {status}"),
        Err(error) => {
            println!("{rung}: {error}");
            for refusal in error.refusals().iter() {
                println!("  {refusal:?}");
            }
        }
    }
}
