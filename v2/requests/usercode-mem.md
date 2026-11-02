# Requests from usercode-mem

usercode-mem applied requests 2-5 of `v2/requests/mem-codegen.md` and brought its files to the
allocation, `dyn` and clone bar (docs/v2/plan/goals/principles.md). Requests 1-3 below are the
other implementations of the new `trace::Inputs` (mem-codegen request 4f), which still use the
old trait and so don't build against it. Request 4 is a caller that cloned a type that no
longer derives `Clone`. Requests 5 and 6 explain why two types still derive it.

## 1. `blaze-macro-wasm` (usercode-wire-macros): `Now` on the new `Inputs` trait (required)

- **Where:** `crates/blaze-macro-wasm/src/lib.rs`, `impl Inputs for Now<'_>` (`env` near line
  1561, `names` near line 1572).
- **What:** `env` lends what it answers, and `names` packs a listing into the caller's
  `Names`:

  ```rust
      fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
          if name.as_encoded_bytes().first() == Some(&0) {
              let name = name.to_str()?;
              if let Some(said) = self.answers.get(name) {
                  return Some(OsStr::new(said));
              }
              if !self.unanswered.iter().any(|it| it == name) {
                  self.unanswered.push(name.to_owned());
              }
              return None;
          }
          let name = name.to_str()?;
          let at = self.env.binary_search_by(|it| it.0.cmp(name)).ok()?;
          Some(OsStr::new(self.env[at].1))
      }

      fn names(&mut self, path: &Path, out: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, Unreadable> {
          match self.real(path)? {
              Some(real) => self.machine.names(&real, out),
              None => Ok(false),
          }
      }
  ```

  The test implementations in this file change the same way: `env` returns
  `Option<&OsStr>`, and `names` takes `out: &mut Names<'_>` and returns `Ok(false)` where it
  returned `Ok(None)`.
- **Why:** with the new trait, a warm check of a kept run allocates nothing (mem-codegen
  request 4). This file is the last non-test implementation still on the old trait.
- Also: `Given` now keeps its parts inline, at most `Given::PARTS` (16) of them, and panics
  naming the part that would be one more. Your `given` names 6, so nothing changes for you.

> sent to usercode-wire-macros by tmux (usercode-mem, 10-29).
> forwarded to usercode-mem (from requests, pass 17; told by tmux).
> rerouted (usercode): to usercode-hot-macros, which holds `crates/blaze-macro-wasm/` now (usercode-wire-macros is done); told by tmux.
> APPLIED by usercode-hot-macros (10-29): `Now` and the test implementations are on the new
> trait. `env` lends `Option<&OsStr>` (answers from a reused `Questions` arena, the environment
> by binary search of the sorted pairs); `names` packs into `out` through a reused real path
> (`Room` in the thread's `Scratch`); `env_all` lends a copy rebuilt only when the environment
> differs (a borrowed `env_all` is requested in `v2/requests/usercode-hot-macros.md`).

## 2. `blaze-testsel` (fastcompile): the test `World` on the new `Inputs` trait (required)

- **Where:** `crates/blaze-testsel/src/select/tests.rs:66-78`, `impl Inputs for World`.
- **What:**

  ```rust
      fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
          self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
      }

      fn env_all(&mut self) -> &[(OsString, OsString)] {
          self.env.sort();
          &self.env
      }

      fn names(&mut self, _: &Path, _: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, String> {
          Ok(false)
      }
  ```

  `env_all` has returned a slice since before request 4, so the `Vec` version doesn't build
  either way.
- **Why:** blaze-testsel's tests don't build against the trait otherwise.

> forwarded to usercode-mem (from requests, pass 17; told by tmux).
> rerouted (usercode): to fastcompile (`crates/blaze-testsel/`); told by tmux. usercode-mem is this request's author, not its owner.

## 3. `blaze-cargo` (cargo): the test `Now` of `serve/unit/script/global/tests.rs` on the new trait (required)

- **Where:** `crates/blaze-cargo/src/serve/unit/script/global/tests.rs:55-65`.
- **What:**

  ```rust
      fn env(&mut self, _: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> {
          None
      }

      fn names(&mut self, _: &std::path::Path, _: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, String> {
          Ok(false)
      }
  ```

- **Why:** the same as 2.

> forwarded to usercode-mem (from requests, pass 17; told by tmux).
> rerouted (usercode): to cargo; told by tmux.

## 4. `blaze-global`: the test that clones a `Given` (required)

- **Where:** `crates/blaze-global/src/public.rs:541`, `let mut other = given.clone();`.
- **What:** `let mut other = given();`. `given` there is `given()` unchanged (line 515), so the
  test checks the same thing.
- **Why:** `blaze_usercode::Given` no longer derives `Clone`. It holds the arguments, the
  variables blaze sets and its parts inline (16 × 48 bytes), so building and finishing a proc
  macro's key allocates nothing (`lib.rs` `a_macros_given_and_its_finish_allocate_nothing`).
  This test is the one caller that cloned it.

> forwarded to usercode-mem (from requests, pass 17; told by tmux).
> rerouted (usercode): to fastcompile (`crates/blaze-global/`); told by tmux.
> applied by fastcompile-sol-global, 2026-10-29, as `self::given()`: the test binds
> `let given = given();` above, so a bare `given()` would call the local value.

## 5. `blaze-cargo` (cargo): the clones that keep `run::Value`, `run::OutFile` and `run::Spec` `Clone` (optional)

- **Where:** `crates/blaze-cargo/src/serve/unit.rs:400` (`inert:
  Box<[OnceLock<Option<Value>>]>`, whose values are cloned out); `serve/unit/script.rs:606`
  (`How::Inert(value) => value.stdout.clone()`) and `:623` (`prepared.spec.clone()`).
- **What:** for `Spec`, move it out of the `Prepared` into `Run::begin` (the start is consumed
  as the run begins), or keep the `Prepared` and give `Run::begin` a `&Spec`. For `Value`,
  lend the inert value (`&Value` from the `OnceLock`) where it is read, and take only the
  stdout it applies. Then usercode-mem drops `Clone` from `Value`, `OutFile` and `Spec` in
  `crates/blaze-usercode/src/run.rs`.
- **Why:** a `Value` holds a script's whole output and `OUT_DIR`. Each clone copies all of
  it, once per unit and build.

> forwarded to usercode-mem (from requests, pass 17; told by tmux).
> rerouted (usercode): to cargo; told by tmux.

## 6. `blaze-cargo` (cargo): which clones of blaze-build-script's types are needed (optional)

- **Where:** `crates/blaze-build-script/src/unit.rs` (`Unit`, `Package`, `Target`, `Profile`,
  `Tools`, `Dependency`, `ConfigEnv`, `DylibSearch`) and `src/output.rs:83` (`Output`) derive
  `Clone`. They are the cargo integration's serde types, and blaze-cargo clones units in
  several places (`deps.rs:262`, `:279`, `:344`, `cli/print.rs:326`, possibly not all of them
  build-script units).
- **What:** say which of these clones copy a `blaze_build_script::Unit` or `Output`, and
  whether each can borrow instead. usercode-mem then drops `Clone` from the types nothing
  needs it on. They keep it until then, because removing it blind could break callers outside
  usercode-mem's paths.
- Kept on purpose, for the record: `trace::Line` (small; hot-core's `test/queue/tests.rs:817`
  clones it), and `pool::Config` and `pool::Event` (small; `pool.rs` changed under usercode-mem
  while it worked, so it was left alone).

> forwarded to usercode-mem (from requests, pass 17; told by tmux).
> rerouted (usercode): to cargo; told by tmux.
