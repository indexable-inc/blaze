//! A test binary that checks blaze ran it on its code memory (docs/v2/plan/design/compile-time.md):
//! `blaze test -p blaze-inproc` links it as a dev executable (blaze-cargo's
//! `inproc::runs_in_process`: it links the dev runtime, here this crate's dependency
//! `blaze-inproc-rt`, and not the `host` feature), and its harness's fork server and every test process it forks start from the file
//! the daemon's code memory wrote, attached by the start hook before `main`.
//!
//! A dev executable's link puts the start hook first among its initializers, so when the hook
//! ran ([`blaze_inproc::runtime::hooked`]) each test fails unless it is attached: a dev
//! executable started from its file (no `BLAZE_INPROC`) or one whose hook could not attach
//! (which exits 127 first) never passes here. Built by cargo the executable has no start hook,
//! and there is nothing to check.

/// The slide of this process's image off its linked address when it runs on the daemon's code
/// memory; `None` when it is no dev executable (built by cargo); why not, when it is one and
/// was not attached.
fn on_code_memory() -> Result<Option<u64>, String> {
    if !blaze_inproc::runtime::hooked() {
        return Ok(None);
    }
    let attached = blaze_inproc::runtime::attach().map_err(|error| {
        format!("a dev executable's test process not on the daemon's code memory: {error}")
    })?;
    if std::env::var_os("BLAZE_INPROC").is_some() {
        return Err(
            "the start hook left `BLAZE_INPROC` in the environment, where the program sees it"
                .to_owned(),
        );
    }
    Ok(Some(attached.slide()))
}

#[test]
fn runs_on_the_daemons_code_memory() {
    if let Err(why) = on_code_memory() {
        panic!("{why}");
    }
}

#[test]
#[ignore = "fails on purpose: `--ignored` checks that blaze reports a failing test run on its code memory as failed"]
fn fails_on_the_daemons_code_memory() {
    let slide = on_code_memory().unwrap_or_else(|why| panic!("{why}"));
    panic!("failed on purpose, on the daemon's code memory (slide {slide:?})");
}
