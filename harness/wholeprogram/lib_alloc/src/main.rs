fn main() {
    let before = counter::allocations();
    let mut text = String::new();
    for it in 0..200 {
        text.push_str(&it.to_string());
    }
    println!("{} characters, {} allocations or reallocations", text.len(), counter::allocations() - before);
}
