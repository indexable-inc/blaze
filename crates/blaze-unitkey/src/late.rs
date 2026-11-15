//! Which variable or file a unit's late read reads: the one rule the command's gather
//! (`blaze_command::front::Command::gather`) and the tests that key real crates answer by, so a
//! test and the command agree on what each read is.
//!
//! - `env!(name)`, `option_env!(name)`: the unit's variable `name` ([`var`]): its value in the
//!   unit's env table (what the unit's plan and its build script's `rustc-env` set), else `1`
//!   for `CARGO_PRIMARY_PACKAGE` of a workspace member, else absent. Absent is a value: the
//!   process's own environment is never read.
//! - `include!(path)`, `include_str!`, `include_bytes!`: the file at `path` when it is
//!   absolute, else at `path` under the directory of the file the read is spelled in
//!   ([`reads`]). The caller reads it; a file that is not there is refused by name.
//!
//! A read whose argument is not a literal (`include!(concat!(env!("OUT_DIR"), "/x.rs"))`) has
//! no rule here: the command refuses it by name until a build script's run is a node whose
//! outputs a read can name. std's units have none at the pinned nightly (its scripts are
//! ported and print no file it includes).
//!
//! Cost: one binary search of the env table per variable read, one copy of the path per file
//! read. Floor: the same.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_keyed::kind::Late;

/// What one late read reads.
#[derive(Debug, PartialEq, Eq)]
pub enum Reads<'n, A: Allocator> {
    /// The unit's variable of this name.
    Var(&'n [u8]),
    /// The file at this path.
    File(Vec<u8, A>),
}

/// What the late read `late` of `name`, spelled in the file at `file` (`/`-separated, as the
/// caller names files), reads.
pub fn reads<'n, A: Allocator>(late: Late, name: &'n [u8], file: &[u8], alloc: A) -> Reads<'n, A> {
    match late {
        Late::Env | Late::OptionEnv => Reads::Var(name),
        Late::Include | Late::IncludeStr | Late::IncludeBytes => {
            let mut path = Vec::new_in(alloc);
            if name.first() != Some(&b'/') {
                let dir = match file.iter().rposition(|&byte| byte == b'/') {
                    Some(at) => &file[..at],
                    None => &[][..],
                };
                path.extend_from_slice(dir);
                path.push(b'/');
            }
            path.extend_from_slice(name);
            Reads::File(path)
        }
    }
}

/// Variable `name` of a unit whose env table is `env` (sorted by name, each name once; `pair`
/// gives a row's name and value), `member` when the unit is a workspace member's.
pub fn var<'e, T>(env: &'e [T], pair: impl Fn(&'e T) -> (&'e [u8], &'e [u8]), member: bool, name: &[u8]) -> Option<&'e [u8]> {
    match env.binary_search_by(|row| pair(row).0.cmp(name)) {
        Ok(at) => Some(pair(&env[at]).1),
        Err(_) if member && name == b"CARGO_PRIMARY_PACKAGE" => Some(b"1"),
        Err(_) => None,
    }
}
