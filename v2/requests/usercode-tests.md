# Requests from usercode-tests

## To codegen: start the fork server from the test harness's `main`

`blaze_usercode::test::FORK_SERVER` is the Rust text of a module `fork_server` (the wire format
of `blaze_usercode::test::wire` and the server over it). It is written for the harness module:
it names libtest as `super::test` and nothing of the crate, and every edition reads it. When
the variable `fork_server::SOCKET` (`__BLAZE_FORK_SERVER`) is set, the binary connects to that
Unix socket and runs tests in forked children as blaze asks, never returning; otherwise libtest
runs as before.

1. `crates/blaze-engine/Cargo.toml`, under `[dependencies]`:

   ```toml
   blaze-usercode.workspace = true
   ```

2. `crates/blaze-engine/src/test_harness.rs`, in `text()`, replace

   ```rust
       text.push_str("];\n        test::test_main_env_args(TESTS)\n    }\n");
       text.push_str(MARKED);
   ```

   with

   ```rust
       text.push_str(
           "];\n        \
            if let ::core::option::Option::Some(socket) = ::std::env::var_os(fork_server::SOCKET) {\n            \
            fork_server::serve(TESTS, socket)\n        \
            }\n        \
            test::test_main_env_args(TESTS)\n    }\n",
       );
       text.push_str(MARKED);
       text.push_str(blaze_usercode::test::FORK_SERVER);
   ```

   so the module's `main` reads:

   ```rust
   pub fn main() -> test::ExitCode {
       const TESTS: &[&test::TestDescAndFn] = &[&T0, &T1, ];
       if let ::core::option::Option::Some(socket) = ::std::env::var_os(fork_server::SOCKET) {
           fork_server::serve(TESTS, socket)
       }
       test::test_main_env_args(TESTS)
   }
   ```

   `serve` returns `!`, so the `if` is `()` in every edition. A crate with no tests still gets
   the module (its `TESTS` is empty and the server answers `Hello` with no names).

3. Any test of `text()` that compares the whole module needs the new `main` and the module
   at its end; the text after `MARKED` is exactly `FORK_SERVER`.

> forwarded to codegen: `crates/blaze-engine/` is codegen's.

## To the sandbox lane

- A forked test's sandbox must allow `connect()` to the Unix socket named in the command's
  `read` set (on macOS a Seatbelt `network-outbound` rule for that one `unix-socket` path; on
  Linux the path is a file in the request directory, already readable).
- The fork server marks each forked child's scope with `access("/.blaze-test/<name>")` before
  the test runs, like `marked::Marked`. Tracing scoped per process after `fork` (every thread
  of the child is that test) would make the scope hold for threads the test spawns too.

> forwarded to usercode: the sandbox is usercode's (`crates/blaze-cargo/src/sandbox*`, usercode-sandbox).

> applied (usercode):
> - **connect:**
>   - Linux: `Recorder::may_connect` also allows a path the read set names exactly (not one
>     below it).
>   - macOS: each read-set path that is a Unix socket when the run starts gets a Seatbelt
>     `(allow network-outbound (remote unix-socket (path-literal (param "SOCKETn"))))` in
>     each of its spellings.
> - **Scope:** a forked process already started in its forker's scope. Now a new thread does
>   too.
>   - Linux: a new thread starts in the scope that every other thread of its process shares,
>     when all are known and in one. This is checked when the thread is first seen and before
>     any thread of the process takes a new mark. Otherwise it gets none, which is every
>     scope's and so sound.
>   - macOS: an interposed `pthread_create` hands the creator's scope to the new thread.
