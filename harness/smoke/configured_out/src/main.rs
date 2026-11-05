fn total(words: &[&str]) -> usize {
    words.iter().map(|it| it.len()).sum()
}

fn main() {
    let words = vec!["configured", "out"];
    println!("total {}", total(&words));
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts() {
        assert_eq!(super::total(&["ab", "c"]), 3);
    }
}
