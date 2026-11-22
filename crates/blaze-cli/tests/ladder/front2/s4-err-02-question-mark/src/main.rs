//! s4-err-02: `?` on `Result` and `Option` against the real `Try` and `FromResidual` impls
//! (core/src/result.rs, option.rs): an error converted by `From`, `Box<dyn Error>` from a `&str`
//! (`impl From<&str> for Box<dyn Error>`), `Error::source`, and `collect` into `Result` and
//! `Option` (the first `Err` or `None` wins).

use std::error::Error;
use std::fmt;
use std::num::ParseIntError;

#[derive(Debug)]
enum AppError {
    Parse(ParseIntError),
    Negative(i64),
    Empty,
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Parse(e) => write!(f, "parse error: {e}"),
            AppError::Negative(n) => write!(f, "{n} is negative"),
            AppError::Empty => write!(f, "empty input"),
        }
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AppError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ParseIntError> for AppError {
    fn from(e: ParseIntError) -> Self {
        AppError::Parse(e)
    }
}

fn parse_positive(s: &str) -> Result<i64, AppError> {
    if s.is_empty() {
        return Err(AppError::Empty);
    }
    let n: i64 = s.trim().parse()?;
    if n < 0 { Err(AppError::Negative(n)) } else { Ok(n) }
}

fn sum_all(items: &[&str]) -> Result<i64, AppError> {
    let mut t = 0;
    for s in items {
        t += parse_positive(s)?;
    }
    Ok(t)
}

fn first_char_upper(s: &str) -> Option<char> {
    let c = s.chars().next()?;
    Some(c.to_ascii_uppercase())
}

fn boxed(s: &str) -> Result<i64, Box<dyn Error>> {
    let n: i64 = s.parse()?;
    if n > 100 {
        return Err("too big".into());
    }
    Ok(n * 2)
}

fn main() {
    println!("{:?}", sum_all(&["1", " 2 ", "3"]).map_err(|e| e.to_string()));
    let bad: [&[&str]; 3] = [&["1", "x"], &["5", "-3"], &["", "1"]];
    for items in bad {
        match sum_all(items) {
            Ok(v) => println!("ok {v}"),
            Err(e) => println!("error: {e} (source: {})", e.source().is_some()),
        }
    }
    println!("{:?} {:?}", first_char_upper("rust"), first_char_upper(""));
    for s in ["7", "700", "q"] {
        match boxed(s) {
            Ok(v) => println!("{v}"),
            Err(e) => println!("{e}"),
        }
    }
    let r: Result<Vec<i32>, _> = ["1", "2", "z"].iter().map(|s| s.parse::<i32>()).collect();
    println!("{}", r.is_err());
    let o: Option<Vec<i32>> = [Some(1), Some(2)].into_iter().collect();
    println!("{:?}", o);
}
