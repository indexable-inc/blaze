//! `unused_parameter`, `versioned_name`, `glob_import` and `shadow_changes_meaning`: where each
//! fires, where it does not, and `glob_import`'s fix.

use super::{fixed, found};

#[test]
fn unused_parameter_finds_a_parameter_nothing_reads_even_with_an_underscore() {
    let out = found(
        "unused_parameter",
        r#"
pub fn scale(value: u32, factor: u32) -> u32 {
    value * 2
}
pub fn ignored(_limit: usize, n: usize) -> usize {
    n
}
"#,
    );
    assert!(out.contains("`fn scale` never reads its parameter `factor`"), "{out}");
    assert!(out.contains("`fn ignored` never reads its parameter `_limit`"), "{out}");
    assert!(!out.contains("`value`"), "{out}");
    assert!(!out.contains("`n`"), "{out}");
}

#[test]
fn unused_parameter_counts_reads_in_macros_and_closures_and_leaves_signatures_others_fix() {
    let out = found(
        "unused_parameter",
        r#"
pub trait Visit {
    fn visit(&self, depth: u32) -> u32 { 0 }
}
pub struct V;
impl Visit for V {
    fn visit(&self, depth: u32) -> u32 { 1 }
}
pub fn checked(n: u32) {
    assert!(n > 0);
}
pub fn later(n: u32) -> impl Fn() -> u32 {
    move || n + 1
}
pub fn stub(n: u32) -> u32 {
    loop {}
}
pub fn callback(n: u32) -> u32 { 0 }
pub fn apply(f: fn(u32) -> u32) -> u32 { f(1) }
pub fn uses() -> u32 { apply(callback) }
#[cfg(not(test))]
pub fn twin(n: u32) -> u32 { 0 }
extern "C" fn foreign(n: u32) -> u32 { 0 }
"#,
    );
    // A callback is reported: that it is one is a fact of a use in another body, which a body's
    // lint does not read; its signature says so with an `#[expect(.., reason)]`.
    assert!(out.contains("`fn callback` never reads its parameter `n`"), "{out}");
    assert_eq!(out.matches("never reads").count(), 1, "{out}");
}

#[test]
fn versioned_name_finds_a_version_beside_its_stem() {
    let out = found(
        "versioned_name",
        r#"
pub fn parse(text: &str) -> u32 { 0 }
pub fn parse_v2(text: &str) -> u32 { 1 }
pub struct Store;
pub struct StoreOld;
pub struct Lexer;
impl Lexer {
    pub fn next(&self) -> u32 { 0 }
    pub fn next_new(&self) -> u32 { 1 }
}
"#,
    );
    assert!(out.contains("`parse_v2` is `parse` with `_v2` on it"), "{out}");
    assert!(out.contains("`StoreOld` is `Store` with `Old` on it"), "{out}");
    assert!(out.contains("`next_new` is `next` with `_new` on it"), "{out}");
}

#[test]
fn versioned_name_leaves_a_lone_number_and_a_deprecated_twin() {
    let out = found(
        "versioned_name",
        r#"
pub fn sha2() -> u32 { 0 }
pub fn level1() -> u32 { 0 }
pub fn read() -> u32 { 0 }
#[deprecated]
pub fn read_old() -> u32 { 0 }
pub fn load_v2() -> u32 { 0 }
"#,
    );
    assert!(!out.contains("with `"), "{out}");
}

#[test]
fn glob_import_finds_a_private_glob_and_its_fix_names_what_is_used() {
    const GLOB: &str = r#"
pub mod shapes {
    pub struct Circle;
    pub struct Square;
    pub fn area() -> u32 { 0 }
}
mod user {
    use crate::shapes::*;
    pub fn make() -> (Circle, u32) { (Circle, area()) }
}
"#;
    let out = found("glob_import", GLOB);
    assert!(out.contains("`use crate::shapes::*` brings every public name"), "{out}");
    let after = fixed("glob_import", GLOB);
    assert!(after.contains("use crate::shapes::{Circle, area};"), "{after}");
}

#[test]
fn glob_import_leaves_reexports_preludes_test_parents_and_variants_in_a_function() {
    let out = found(
        "glob_import",
        r#"
pub mod prelude {
    pub struct P;
}
pub mod inner {
    pub struct I;
}
pub use inner::*;
use prelude::*;
pub enum Op { Add, Sub }
pub fn apply(op: Op) -> u32 {
    use Op::*;
    match op { Add => 1, Sub => 2 }
}
pub fn p() -> P { P }
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() { let _ = apply(Op::Add); }
}
"#,
    );
    assert!(!out.contains("brings every public name"), "{out}");
}

#[test]
fn shadow_changes_meaning_finds_an_unrelated_shadow_and_leaves_a_refining_one() {
    let out = found(
        "shadow_changes_meaning",
        r#"
pub fn sizes(header: &[u8], body: &[u8]) -> usize {
    let len = header.len();
    let first = len;
    let len = body.len();
    first + len
}
pub fn refined(text: Option<u32>) -> u32 {
    let text = text.unwrap_or(0);
    let n = 1;
    let n = n + 1;
    let shown = 3;
    let shown = { let at = shown; at * 2 };
    text + n + shown
}
pub fn scoped() -> u32 {
    let a = { let v = 1; v };
    let v = 2;
    a + v
}
"#,
    );
    assert!(out.contains("`let len` shadows a `len` still in scope"), "{out}");
    assert!(!out.contains("`let text`"), "{out}");
    assert!(!out.contains("`let n`"), "{out}");
    assert!(!out.contains("`let shown`"), "{out}");
    assert!(!out.contains("`let v`"), "{out}");
}
