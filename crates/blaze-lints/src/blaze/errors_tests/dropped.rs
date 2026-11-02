//! `dropped_error` over [`super`]'s fixtures.

use super::check;

/// What `dropped_error` finds in every test of it: `Err` arms and methods of `Result` that throw
/// the error away, under a renamed `Result` and through a macro, beside the near misses that
/// keep or hand on the error, or end the function.
const DROPPED: &str = r#"
use core::result::Result as Outcome;
pub struct ReadError;
pub fn read() -> Outcome<u32, ReadError> {
    Ok(1)
}
macro_rules! quietly {
    ($e:expr) => {
        $e.ok()
    };
}
pub fn arms() -> Option<u32> {
    let a = match read() {
        Ok(it) => it,
        Err(_) => 0, //~ dropped_error
    };
    let b = match read() {
        Ok(it) => it,
        Err(..) => 0, //~ dropped_error
    };
    let c = match read() {
        Ok(it) => it,
        Err(_unused) => 0, //~ dropped_error
    };
    let d = match read() {
        Ok(it) => it,
        Err(_) => return None,
    };
    match read() {
        Ok(_) => {}
        Err(_) => {}
    }
    let e = match read() {
        Ok(it) => it,
        Err(error) => keep(error),
    };
    Some(a + b + c + d + e)
}
pub fn keep(_error: ReadError) -> u32 {
    0
}
pub fn methods() -> Outcome<u32, u32> {
    let _ = read().ok(); //~ dropped_error
    let _ = read().unwrap_or(0); //~ dropped_error
    let _ = read().unwrap_or_default(); //~ dropped_error
    let _ = read().unwrap_or_else(|_| 0); //~ dropped_error
    let _ = read().unwrap_or_else(keep);
    let _ = read().map_err(|_| 1u32); //~ dropped_error
    let _ = read().map_err(|_why| 1u32); //~ dropped_error
    let _ = read().map_err(|error| keep(error));
    let _ = quietly!(read()); //~ dropped_error
    read().ok(); //~ dropped_error
    read().map_err(|_| 1u32) //~ dropped_error
}
#[expect(blaze::dropped_error, reason = "the count is a hint; a failed read is no count")]
pub fn expected() -> u32 {
    read().unwrap_or(0)
}
"#;

#[test]
fn dropped_error() {
    let out = check(DROPPED, "-A clippy::all --warn=blaze::dropped_error");
    assert!(!out.contains("unfulfilled"), "the `#[expect]` is met:\n{out}");
    assert!(out.contains("`ReadError`"), "a finding names the error dropped:\n{out}");
}

/// With `clippy::unused_result_ok` and `clippy::map_err_ignore` on, the places they report are
/// theirs: `read().ok();` and the last `map_err(|_| ..)`s are not reported twice.
#[test]
fn dropped_error_leaves_clippys_places_to_clippy() {
    let source = DROPPED
        .replace("    read().ok(); //~ dropped_error", "    read().ok();")
        .replace("let _ = read().map_err(|_| 1u32); //~ dropped_error", "let _ = read().map_err(|_| 1u32);")
        .replace("    read().map_err(|_| 1u32) //~ dropped_error", "    read().map_err(|_| 1u32)");
    let out = check(
        &source,
        "-A clippy::all --warn=blaze::dropped_error --warn=clippy::unused_result_ok --warn=clippy::map_err_ignore",
    );
    let clippy = out.lines().filter(|it| it.starts_with("warning: `map_err(|_| ..)` throws away")).count();
    assert_eq!(clippy, 2, "map_err_ignore reports both `|_|` closures:\n{out}");
}
