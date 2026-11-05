//! `untyped_error` over [`super`]'s fixtures, with an `alloc` and an `anyhow` of their own.

use super::check_in;

/// An `alloc` (not `std`, whose prelude the fixture would look for) with the lang items `String`
/// and `owned_box`, and an `anyhow` whose `Result` is an alias with a default error.
const ERROR_CRATES: &str = r#"
//- /alloc/lib.rs crate:alloc
#![feature(lang_items)]
#[lang = "String"]
pub struct String;
#[lang = "owned_box"]
pub struct Box<T: ?Sized>(*mut T);
//- /anyhow/lib.rs crate:anyhow
pub struct Error;
pub type Result<T, E = Error> = core::result::Result<T, E>;
"#;

/// A library's signatures: `String`, `Box<dyn Error>` and `anyhow::Error` errors anywhere in a
/// return or parameter type are findings, renamed or written by a macro; a typed error, a
/// `String` that is no error and a trait impl's method (found at the trait) are none.
#[test]
fn untyped_error_in_a_library() {
    let crates = format!("{ERROR_CRATES}//- /src/lib.rs crate:main deps:hir_ty,alloc,anyhow\n");
    check_in(
        &crates,
        r#"
use alloc::String as Text;
use alloc::Box;
pub enum ParseError {
    Empty,
}
pub fn parse(_s: &str) -> Result<u32, alloc::String> { //~ untyped_error
    Ok(0)
}
pub fn renamed() -> Result<u32, Text> { //~ untyped_error
    Ok(0)
}
pub fn boxed() -> Result<(), Box<dyn Error>> { //~ untyped_error
    Ok(())
}
pub fn any() -> anyhow::Result<u32> { //~ untyped_error
    Ok(0)
}
pub fn takes(_r: Result<u32, Text>) {} //~ untyped_error
pub fn nested() -> Option<Result<u32, Text>> { //~ untyped_error
    None
}
pub fn typed() -> Result<u32, ParseError> {
    Ok(0)
}
pub fn text() -> Text {
    alloc::String
}
pub trait Load {
    fn load(&self) -> Result<u32, Text>; //~ untyped_error
}
pub struct Disk;
impl Load for Disk {
    fn load(&self) -> Result<u32, Text> {
        Ok(0)
    }
}
macro_rules! make {
    () => {
        pub fn made() -> Result<u32, Text> {
            Ok(0)
        }
    };
}
make!(); //~ untyped_error
"#,
        "-A clippy::all --warn=blaze::untyped_error",
    );
}

/// A binary's crate root is its `main` module, which may report to a person and exit: an
/// `anyhow::Result` there is none, and one in another of its modules is a finding.
#[test]
fn untyped_error_in_a_binary_is_allowed_in_its_main_module() {
    let crates = format!("{ERROR_CRATES}//- /src/lib.rs crate:bin:main deps:hir_ty,alloc,anyhow\n");
    check_in(
        &crates,
        r#"
pub fn main() -> anyhow::Result<()> {
    cli::run()
}
mod cli {
    pub fn run() -> anyhow::Result<()> { //~ untyped_error
        Ok(())
    }
}
"#,
        "-A clippy::all --warn=blaze::untyped_error",
    );
}
