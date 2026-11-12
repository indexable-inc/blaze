fn f(x: u32) -> u32 {
    x + 1
}

fn g(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", f(1), g(2));
}

#[cfg(test)]
mod tests {
    #[test]
    fn f_adds_one() {
        assert_eq!(super::f(1), 2);
    }
}
