fn main() {
    let point = app::make(7);
    println!("{} {} {}", app::read(&point), std::mem::size_of::<app::Point>(), app::unrelated(2));
}
