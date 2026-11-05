fn main() {
    println!("{}", soak_core::summary());
    for (name, value) in soak_core::names() {
        println!("{name} {value}");
    }
}
