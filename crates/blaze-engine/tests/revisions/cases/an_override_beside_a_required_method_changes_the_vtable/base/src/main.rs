// note 0
fn local() -> u32 {
    300
}

fn main() {
    let unused_at_the_far_end = 9;
    println!("{} {} {} {} {}", app::entry(), local(), 0, 0, 0);
}
