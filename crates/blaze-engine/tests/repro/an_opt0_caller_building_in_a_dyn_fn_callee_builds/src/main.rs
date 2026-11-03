/// The program's own type, so `dep::resize::<K>` is the workspace's code and its own unit is
/// built against the program's dyn facts.
struct K;

fn main() {
    let k = std::env::args().count() as u32;
    println!("{}", dep::resize::<K>(hot::one(40), &|y| y + k));
}
