/// The program's own type, so `dep::outer::<K>` and `dep::resize::<K>` are the workspace's
/// code and their own units are built against the program's dyn facts.
struct K;

fn main() {
    let k = std::env::args().count() as u32;
    println!("{}", dep::outer::<K>(41, &|y| y + k));
}
