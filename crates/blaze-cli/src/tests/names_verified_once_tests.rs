//! A dependency's exports record is checked once for every crate of the request's names phase
//! that reads it, never once per reader (rule 74; blaze-scope scope_record.rs `Verified`). A
//! child of main.rs's tests: its helpers are theirs.

use super::*;
use crate::engine_names::CHECKED;

/// A virtual workspace of 21 members: `hub`, whose lib is `pub mod m { pub fn f() {} }`, and
/// `r00` to `r19`, each depending on it by path and calling `hub::m::f`. The twenty readers are
/// one names phase (each one edge deep); the phase maps `hub`'s exports record once into its
/// slot and checks it there (engine_names.rs `checked`), and each reader opens that proof.
/// So `hub`'s record is checked exactly once, and every reader resolves (no failure, no error).
///
/// The control (rule 23): `checked` called per dependent instead (in `Job::run`, where
/// blaze-scope's `resolve_crate` checked each handed record before 2026-11-21) counts 20.
///
/// Expected, from `cargo test -p blaze-cli --bin blaze names_verified_once`:
/// `test tests::names_verified_once_tests::a_record_read_by_twenty_crates_is_checked_once ... ok`.
#[test]
fn a_record_read_by_twenty_crates_is_checked_once() {
    const READERS: usize = 20;
    {
        let mut checks = CHECKED.lock();
        checks.krate = "hub";
        checks.count = 0;
    }
    let io = Io::simulated(SimWorld::new(19, Limits::USUAL));
    let readers: std::vec::Vec<std::string::String> = (0..READERS).map(|i| std::format!("r{i:02}")).collect();
    let members: std::vec::Vec<std::string::String> = core::iter::once(std::string::String::from("\"hub\"")).chain(readers.iter().map(|r| std::format!("\"{r}\""))).collect();
    write(&io, "Cargo.toml", &std::format!("[workspace]\nmembers = [{}]\nresolver = \"2\"\n", members.join(", ")));
    write(&io, "hub/Cargo.toml", "[package]\nname = \"hub\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "hub/src/lib.rs", "pub mod m { pub fn f() {} }\n");
    for r in readers.iter() {
        write(&io, &std::format!("{r}/Cargo.toml"), &std::format!("[package]\nname = \"{r}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nhub = {{ path = \"../hub\" }}\n"));
        write(&io, &std::format!("{r}/src/lib.rs"), "pub fn h() {\n    hub::m::f()\n}\n");
    }
    let checked = check(&io);
    let out = stdout(&io);
    let count = {
        let mut checks = CHECKED.lock();
        checks.krate = "";
        core::mem::replace(&mut checks.count, 0)
    };
    assert_eq!(checked.failures, 0, "{out}\n{}", checked.profile);
    assert_eq!(checked.errors, 0, "{out}\n{}", checked.profile);
    assert_eq!(count, 1, "`hub`'s exports record was checked {count} times for {READERS} readers:\n{out}\n{}", checked.profile);
    println!("`hub`'s exports record checked {count} time for {READERS} readers");
}
