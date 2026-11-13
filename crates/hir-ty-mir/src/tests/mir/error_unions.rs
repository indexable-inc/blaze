//! Error unions (`#![blaze::error_unions]`, docs/v2/plan/design/error-sets.md) as programs
//! that run: `?` widens a member or a smaller union into a function's error set through
//! core's `From`, a coercion does the same, and the value that comes out is the member that
//! went in. The errors are shaped as bstd's (`io::Missing`, `fs::FsError`, `Stopped`).

use super::eval::check_pass;

/// bstd's errors, reduced to their shapes, and the calls that fail with them.
const BSTD: &str = r#"
pub struct Missing { pub capability: &'static str }
pub enum FsKind { NotFound, NotADirectory }
pub struct FsError { pub op: u8, pub kind: FsKind }
pub enum Stopped { Deadline { at: u32 }, Cancelled }

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

pub struct Fs;
fn need_fs(offered: bool) -> Result<Fs, Missing> {
    if offered { Ok(Fs) } else { Err(Missing { capability: "fs" }) }
}
impl Fs {
    fn open_dir(&self, there: bool) -> Result<u8, FsError> {
        if there { Ok(7) } else { Err(FsError { op: 3, kind: FsKind::NotFound }) }
    }
}
fn sleep(cancelled: bool) -> Result<(), Stopped> {
    if cancelled { Err(Stopped::Cancelled) } else { Ok(()) }
}
"#;

fn program(main: &str) -> String {
    format!(
        "//- minicore: sized, error_union, result, try, from, fmt, error, clone, hash, str, slice\n\
         //- /main.rs crate:main\n#![blaze::error_unions]\n{BSTD}\n{main}"
    )
}

/// (a) of the feature's check: one function mixes `need_fs()?` (`Missing`) and
/// `open_dir()?` (`FsError`) with no glue, and each failure comes out as the member it went
/// in as, with its fields.
#[test]
fn question_mark_widens_two_bstd_errors_into_one_set_and_runs() {
    check_pass(&program(
        r#"
fn run(offered: bool, there: bool) -> Result<u8, Missing | FsError> {
    let fs = need_fs(offered)?;
    let dir = fs.open_dir(there)?;
    Ok(dir)
}

fn main() {
    match run(true, true) {
        Ok(7) => {}
        _ => should_not_reach(),
    }
    match run(false, true) {
        Err(Missing { capability }) => {
            if capability.as_bytes().len() != 2 {
                should_not_reach();
            }
        }
        Err(FsError { .. }) => should_not_reach(),
        Ok(_) => should_not_reach(),
    }
    match run(true, false) {
        Err(FsError { op: 3, kind: FsKind::NotFound }) => {}
        Err(FsError { .. }) => should_not_reach(),
        Err(Missing { .. }) => should_not_reach(),
        Ok(_) => should_not_reach(),
    }
}
"#,
    ));
}

/// A smaller set widens into a larger one by `?`, a member by coercion (`Err(e)`, a typed
/// `let`), and a member that is an enum is matched by its variants; whichever order the
/// sets are written in.
#[test]
fn a_subset_widens_and_an_enum_member_is_matched_by_its_variants() {
    check_pass(&program(
        r#"
fn run(offered: bool, there: bool) -> Result<u8, FsError | Missing> {
    let fs = need_fs(offered)?;
    Ok(fs.open_dir(there)?)
}

fn run_then_sleep(offered: bool, there: bool, cancelled: bool) -> Result<u8, Stopped | Missing | FsError> {
    let dir = run(offered, there)?;
    sleep(cancelled)?;
    if dir == 0 {
        return Err(Stopped::Deadline { at: 9 });
    }
    Ok(dir)
}

fn which(r: Result<u8, Missing | FsError | Stopped>) -> u8 {
    match r {
        Ok(dir) => dir,
        Err(Missing { .. }) => 1,
        Err(FsError { kind: FsKind::NotADirectory, .. }) => 2,
        Err(FsError { op, .. }) => op,
        Err(Stopped::Cancelled) => 4,
        Err(Stopped::Deadline { at }) => at as u8,
    }
}

fn main() {
    if which(run_then_sleep(true, true, false)) != 7 { should_not_reach(); }
    if which(run_then_sleep(false, true, false)) != 1 { should_not_reach(); }
    if which(run_then_sleep(true, false, false)) != 3 { should_not_reach(); }
    if which(run_then_sleep(true, true, true)) != 4 { should_not_reach(); }
    let direct: Missing | Stopped = Stopped::Deadline { at: 9 };
    let n = match direct {
        Missing { .. } => 0,
        Stopped::Deadline { at } => at,
        Stopped::Cancelled => 1,
    };
    if n != 9 { should_not_reach(); }
}
"#,
    ));
}

/// A member is matched through a reference to the union (the bindings borrow), bound whole
/// with `@`, and in a `let`-`else` and an `if let`.
#[test]
fn a_member_is_matched_by_reference_bound_whole_and_in_let_else() {
    check_pass(&program(
        r#"
fn capability(e: &(Missing | FsError)) -> usize {
    match e {
        Missing { capability } => capability.as_bytes().len(),
        FsError { op, .. } => *op as usize,
    }
}

fn op_of(e: FsError) -> u8 { e.op }

fn whole(e: Missing | FsError) -> u8 {
    match e {
        m @ Missing { .. } => m.capability.as_bytes().len() as u8,
        f @ FsError { .. } => op_of(f),
    }
}

fn only_fs(e: Missing | FsError) -> u8 {
    let FsError { op, .. } = e else { return 0 };
    op
}

fn main() {
    let missing: Missing | FsError = Missing { capability: "net" };
    let fs: Missing | FsError = FsError { op: 5, kind: FsKind::NotFound };
    if capability(&missing) != 3 { should_not_reach(); }
    if capability(&fs) != 5 { should_not_reach(); }
    if let Missing { capability } = &missing {
        if capability.as_bytes().len() != 3 { should_not_reach(); }
    } else {
        should_not_reach();
    }
    if only_fs(Missing { capability: "net" }) != 0 { should_not_reach(); }
    if only_fs(FsError { op: 6, kind: FsKind::NotFound }) != 6 { should_not_reach(); }
    if whole(missing) != 3 { should_not_reach(); }
    if whole(fs) != 5 { should_not_reach(); }
}
"#,
    ));
}
