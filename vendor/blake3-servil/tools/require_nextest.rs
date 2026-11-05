// Included (include!) by every crate's tests in this repository: under
// `cargo test` this test fails with how to run them, and cargo stops at
// the first binary that fails. The tests run under cargo-nextest, each in
// its own process (AGENTS: "Tests run under nextest"), and some rely on
// being alone in theirs.
#[test]
fn aaa_require_nextest() {
    if std::env::var_os("NEXTEST").is_none() {
        panic!(
            "\n\n\x1b[1;31merror:\x1b[0m these tests run under cargo-nextest, each in its own process.\n\n\
             \x20   Run them with:     \x1b[1;32mcargo nextest run\x1b[0m\n\
             \x20   The doc examples:  cargo test --doc\n\
             \x20   Install nextest:   https://nexte.st (or cargo install cargo-nextest --locked)\n"
        );
    }
}
