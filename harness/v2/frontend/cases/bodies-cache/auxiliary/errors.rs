//@ edition: 2021
use std::fmt;
#[derive(Debug, PartialEq)]
pub enum ParseError { Empty, NotDigit(char) }
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { ParseError::Empty => write!(f, "empty"), ParseError::NotDigit(c) => write!(f, "not a digit: {c}") }
    }
}
pub fn parse(s: &str) -> Result<u32, ParseError> {
    if s.is_empty() { return Err(ParseError::Empty); }
    let mut n = 0u32;
    for c in s.chars() { let d = c.to_digit(10).ok_or(ParseError::NotDigit(c))?; n = n * 10 + d; }
    Ok(n)
}
pub fn sum_all(items: &[&str]) -> Result<u32, ParseError> { let mut t = 0; for it in items { t += parse(it)?; } Ok(t) }
