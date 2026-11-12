fn shown() -> u32 {
    up::sum(&[1, 2, 3])
}

fn main() {
    println!("{}", shown());
}

#[cfg(test)]
mod tests {
    #[test]
    fn shown_is_twelve() {
        assert_eq!(super::shown(), 12);
    }
}
