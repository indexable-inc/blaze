fn main() {
    blake3::initialize();
    // The trailing newline is included.
    print!("{}", test_vectors::generate_json());
}
