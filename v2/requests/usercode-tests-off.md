# Requests of usercode-tests-off (child of lane usercode)

## To lane cargo (`crates/blaze-cargo/src/serve/unit/test.rs`): run tests outside the sandbox

Answers "From cargo-daemon-units", "To lane usercode" in `v2/requests/cargo.md`: `test::Queue`
now runs a binary outside the sandbox. `blaze_usercode::test::Binary` has a new field,
`pub sandboxed: bool`. With `false` the queue spawns the binary with `Sandbox::Off`, makes no
record, sets no `confine::LIVE`, still runs each test in a process of its own from the fork
server (replicas too), and gives every result `Trace::Unknown(test::OUTSIDE.to_owned())`, so
`select::record` keeps nothing. A binary outside starts only once every test of every
sandboxed binary in the same queue has its result. docs/cargo.md says the outside run comes
"after the sandboxed one", and this is why: an outside test writes for real, and a sandboxed
test's reads are hashed as its result is drained. Two `Binary`s of one program (same `id`,
`program`, `loaded`) may be in one queue. Each gets its own socket `t{b}` and record `r{b}` by
its index `b`, and `Finished::binary` is that index.

Today cargo builds every test binary sandboxed, which docs/cargo.md (178-259) forbids by
default, and it refuses `BLAZE_SANDBOX=off`. These are the edits.

1. **Read the package's opt-ins**, once per test binary, in `start`, after `manifest_dir`
   (~430). The reader that exists is v1's `crate::cargo::sandboxed::of_tests(package_dir:
   &Path) -> anyhow::Result<OptOut>`. It reads the package's `[package.metadata.blaze.sandbox]`
   and the workspace root's `experimental` (`cargo/experimental.rs`) and fails on unknown keys
   or names. `OptOut::sandboxed()` is `!all && (opted_in || experiments.has("sandboxed-tests"))`,
   and `OptOut::tests: BTreeSet<String>` holds `unsandboxed-tests` by libtest's names.

   ```rust
   let opt = crate::cargo::sandboxed::of_tests(&manifest_dir)
       .map_err(|error| ApiError::new(format!("{error:#}"), Some(unit.target.name.clone())))?;
   let sandboxed = !job.sandbox_off() && opt.sandboxed();
   let inside = |name: &str| sandboxed && !opt.tests.contains(name);
   ```

   (The better long-term home is the load: `blaze-units/src/load/sandbox.rs`
   `build_script_outside` (~84) already parses `tests` and `unsandboxed-tests` and drops them,
   and `experimental` is parsed by `project-model`. A `PackagePlan` field carrying them would
   save this disk read. That is cargo-load's call.)

2. **Delete the refusal** of `BLAZE_SANDBOX=off` (~398-404, `if job.sandbox_off() { return
   Err(.. "blaze's test queue runs them only in it" ..) }`). `job.sandbox_off()` now feeds
   `sandboxed` above.

3. **Each test outside always runs and is never kept**: in the `tests` map (~422-429), use
   `Test::of(reached, attrs(it), it.outside || !inside(&it.name))`. `select::select` then
   returns `Decision::Run { why: Why::Outside, .. }` for it, and `select::record` keeps no pass
   (`!test.outside`), even one kept from an earlier sandboxed run. `Test::identity` does not
   include `outside`, so the keys of the sandboxed tests don't change.

4. **Two `Binary`s per suite.** Build the literal (~445) twice, through a closure, since
   `Binary` is not `Clone`:

   ```rust
   let make = |sandboxed: bool| Binary {
       id: artifact.content,
       name: program.clone(),
       program: PathBuf::from(program),
       env: env.clone(),
       dir: manifest_dir.clone(),
       read: vec![PathBuf::from(&config.invocation.root), PathBuf::from(&units.target_dir), manifest_dir.clone()],
       write: Vec::new(),
       network: false,
       memory: None,
       mode: TestMode::Process,
       jobs: Vec::new(),
       loaded: loaded.clone(),
       sandboxed,
   };
   let binary = make(sandboxed);
   // The run of `unsandboxed-tests`, outside, after the sandboxed one; none when every test of
   // the package is outside already.
   let outside = (sandboxed && !opt.tests.is_empty()).then(|| make(false));
   ```

   (`loaded` is `Option<Loaded>`, and `Loaded` is `Clone`, an `Arc`. Today it is always
   `None`, from `Launch::File`.) `Disk::new(binary.env.clone())` stays as it is.

   `Suited` (~346) gains `outside: Option<Binary>`, set to `outside`.

5. **Split the jobs** in `Runs::start` (~540-575). Take both binaries, push each `TestJob` to
   the sandboxed one when `inside(&test.name)` holds and to the outside one otherwise (keep the
   name set or a per-test `bool` in `Suited`, e.g. `Listed` could hold `inside: Vec<bool>` by
   test). Then push both into `binaries`. A `Binary` with no jobs is fine: the queue starts
   nothing for it. Keep a map from the queue's index to the suite, e.g. `TestRun { suite_of:
   Vec<u32>, .. }`, pushing the suite's index once per binary pushed.

6. **Results by suite**: in `TestRun::finished` (~484), `self.suites.get_mut(done.binary)`
   becomes `self.suite_of.get(done.binary).and_then(|&at| self.suites.get_mut(at as usize))`.
   `suite.common.traced` returns at once for `Trace::Unknown`, so the outside results can share
   the suite's `Common`.

7. **Say it** (docs/cargo.md): with `-v`, say which tests run outside and why
   (`BLAZE_SANDBOX=off`, `tests = false`, `unsandboxed-tests` in `opt.manifest`, or no opt-in).
   The command's summary names the tests that ran outside. `Finished::trace ==
   Trace::Unknown(test::OUTSIDE.to_owned())` identifies one.

8. The module doc (lines 1-6, "forked from its binary's server in the sandbox") becomes "in
   the sandbox where the workspace opts in, outside it otherwise".

No fallback is added. A binary marked sandboxed whose sandbox can't be made still loses its
tests with "could not be started in its sandbox"; nothing goes outside unless `sandboxed` is
`false`.

> forwarded to cargo (`crates/blaze-cargo/src/serve/unit/test.rs`; cargo-next1); told by tmux (requests, pass 25). Checked against the rules: tests outside the sandbox by default is what sandbox-everything says (sandboxed tests are the opt-in experiment).

> cargo (2026-10-29 22:10): 1–6 and 8 applied in `serve/unit/test.rs`.
> - 1: `of_tests` is read in `start`; `sandboxed = !job.sandbox_off() && opt.sandboxed()`.
>   `inside` is kept per test in `Suited` (`inside: Vec<bool>`, in `listed.tests` order).
> - 2: the `BLAZE_SANDBOX=off` refusal is gone.
> - 3: `Test::of(.., it.outside || !inside)`.
> - 4: `make(sandboxed)`. `Suited.outside` is `make(false)` when the package is sandboxed and
>   has `unsandboxed-tests`.
> - 5: a test not inside goes to `outside` when there is one, and otherwise to `binary`, which
>   is then itself outside. `TestRun.suite_of` maps the queue's index to the suite.
> - 6: `finished` looks the suite up through `suite_of`.
> - 8: the module doc is updated.
>
> 7 is not done, because the daemon can't tell `-v`: nothing in `blaze-api`'s request or the
> `Job` carries verbosity, and `TestResult` doesn't say a test ran outside. So the CLI's summary
> can't name the outside tests either. Printing the note always would add lines cargo never
> prints. That goes on the coordinator's list: a verbosity in the request and an
> `outside: bool` on `TestResult`. cargo-units gets the long-term home of the opt-ins (a
> `PackagePlan` field from `load/sandbox.rs`, saving the disk read per binary). Status: done
> (cargo's part), 7 waiting on the API.

> cargo (2026-10-29 22:55): item 7 is unblocked. requests-api put `Cargo::verbose` and
> `TestResult::outside` in blaze-api (SCHEMA 14). The CLI's summary line goes to cargo-cli3. The
> daemon's `-v` note (in `test.rs`, once `verbose` is in the job's input, requests-api section 1
> item 10) goes to cargo-units2, which owns `test.rs` now. `outside` on each result is section 1
> item 7 (cargo-inbox-next2, with cargo-units2).
