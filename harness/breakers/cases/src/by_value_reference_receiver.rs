//! Reduced from a string-separator trait: a by-value reference receiver contributes no
//! receiver lifetime. The returned slices borrow the input, not the separator.

trait Tokenize {
    fn tokens(self, input: &str) -> Vec<&str>;
}

impl Tokenize for &str {
    fn tokens(self, input: &str) -> Vec<&str> {
        input.split(self).collect()
    }
}

pub fn run() -> String {
    let input = String::from("red::green::blue");
    let tokens = {
        let separator = String::from("::");
        separator.as_str().tokens(&input)
    };
    tokens.join("|")
}
