//! Error unions (`#![blaze::error_unions]`, docs/v2/plan/design/error-sets.md) in
//! diagnostics: a `?` whose failure the function's error set does not cover names the call
//! that fails with it, the member and the set, and the fix is the set with the member added;
//! a `match` that leaves a member out names the member.

use crate::tests::checked;

/// bstd's errors, reduced to their shapes, and the calls that fail with them.
const BSTD: &str = r#"
pub struct Missing { pub capability: &'static str }
pub struct FsError { pub op: u8 }
pub enum Stopped { Deadline { at: u32 }, Cancelled }
pub struct Fs;
pub fn need_fs() -> Result<Fs, Missing> { loop {} }
impl Fs {
    pub fn open_dir(&self) -> Result<u8, FsError> { loop {} }
}
pub fn sleep() -> Result<(), Stopped | Missing> { loop {} }
"#;

fn stderr(main: &str) -> String {
    let fixture = format!(
        "//- minicore: sized, error_union, result, try, from, fmt, error, clone, hash\n\
         //- /main.rs crate:main\n#![blaze::error_unions]\n{BSTD}\n{main}"
    );
    checked(&fixture).stderr()
}

/// (b) of the feature's check: `fs.open_dir()?` fails with `FsError`, which `run`'s error
/// type, `Missing`, does not hold. The error names the call that adds the failure, the
/// member, the set as the signature writes it and whose it is, and its fix is the set with
/// the member added. Nothing of rustc's unmet `From` bound is said.
#[test]
fn a_question_mark_whose_failure_the_set_lacks_names_the_call_the_member_and_the_set() {
    let out = stderr(
        r#"
pub fn run() -> Result<u8, Missing> {
    let fs = need_fs()?;
    let dir = fs.open_dir()?;
    Ok(dir)
}
"#,
    );
    let expected = "\
error[E0277]: `fs.open_dir()?` can fail with `FsError`, which the error set of `run`, `Missing`, does not have
  --> main.rs:16:15
   |
14 | pub fn run() -> Result<u8, Missing> {
   |                            ------- the error set of `run`
15 |     let fs = need_fs()?;
16 |     let dir = fs.open_dir()?;
   |               ^^^^^^^^^^^^^^ fails with `FsError`
   |
help: add `FsError` to the set, or handle it at the call
   |
14 | pub fn run() -> Result<u8, Missing | FsError> {
   |                                    +++++++++
";
    assert!(out.starts_with(expected), "{out}");
    assert!(!out.contains("From<"), "{out}");
}

/// A call that fails with a set: only the members the function's set lacks are named, and
/// the set is said as the signature writes it, whatever its canonical order.
#[test]
fn a_smaller_set_names_only_the_members_missing() {
    let out = stderr(
        r#"
pub fn run() -> Result<u8, Missing | FsError> {
    need_fs()?;
    sleep()?;
    Ok(1)
}
"#,
    );
    let expected = "\
error[E0277]: `sleep()?` can fail with `Stopped`, which the error set of `run`, `Missing | FsError`, does not have
  --> main.rs:16:5
   |
14 | pub fn run() -> Result<u8, Missing | FsError> {
   |                            ----------------- the error set of `run`
15 |     need_fs()?;
16 |     sleep()?;
   |     ^^^^^^^^ fails with `Stopped`
   |
help: add `Stopped` to the set, or handle it at the call
   |
14 | pub fn run() -> Result<u8, Missing | FsError | Stopped> {
   |                                              +++++++++
";
    assert!(out.starts_with(expected), "{out}");
}

/// The control: with the member in the set, the same body has no error.
#[test]
fn a_set_that_has_every_failure_is_accepted() {
    let out = stderr(
        r#"
pub fn run() -> Result<u8, Missing | FsError | Stopped> {
    let fs = need_fs()?;
    let dir = fs.open_dir()?;
    sleep()?;
    Ok(dir)
}
"#,
    );
    assert_eq!(out, "");
}

/// (d): a `match` that leaves a member out is refused naming the member as the pattern that
/// covers it, the matched type is written as the union, and no note points into core.
#[test]
fn a_match_that_leaves_a_member_out_names_the_member() {
    let out = stderr(
        r#"
pub fn code(e: Missing | FsError) -> u8 {
    match e {
        Missing { .. } => 0,
    }
}
"#,
    );
    let expected = "\
error[E0004]: non-exhaustive patterns: `FsError { .. }` not covered
  --> main.rs:15:11
   |
15 |     match e {
   |           ^ pattern `FsError { .. }` not covered
   |
   = note: the matched value is of type `FsError | Missing`
";
    assert!(out.starts_with(expected), "{out}");
    assert!(!out.contains("defined here") && !out.contains("ErrorUnion"), "{out}");
}
