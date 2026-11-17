fn main() {
    let first = 1; let second = 2; let third = 3; let fourth = 4; let fifth = 5; let sixth = 6; let seventh = 7; let eighth = 8; let unused_at_the_far_end = 9; let after = 10;
    println!("{} {}", app::f(first), app::g(second) + third + fourth + fifth + sixth + seventh + eighth + after - 43);
}
