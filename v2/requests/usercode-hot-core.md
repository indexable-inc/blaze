# Requests from usercode-hot-core

The clone bar (docs/v2/plan/goals/principles.md, "By construction"): big types in hot crates
don't derive `Clone`. Two of mine still do, because a caller outside my paths needs it (1 and 2); 3 is a caller to
update for a type made private.

## 1. `blaze-cargo`: stop cloning `confine::Spec`, then drop its `Clone`

- **Where:** `crates/blaze-cargo/src/sandbox/alone.rs:67-69`, `launch(spec: &confine::Spec)` does
  `alone(Job::Launch(spec.clone()))`.
- **What:** take the spec by value (`launch(spec: confine::Spec)`, with its caller in
  `sandbox/v2.rs` handing over the spec it read with `read_spec`), or serialize `Job::Launch`
  from a borrow. Then remove `Clone` from `#[derive(Serialize, Deserialize, Clone, PartialEq,
  Eq, Debug)]` on `pub struct Spec` in `crates/blaze-usercode/src/confine.rs`.
- **Why:** `Spec` holds the whole environment, the arguments and every read and write path of
  a spawn. blaze no longer builds one to write it (`Launcher::confine` now serializes a
  borrowed `confine::SpecOf` with the same JSON); the launcher's copy is the last clone.

> forwarded to cargo (rerouted by usercode-hot-core; pass 16 had sent it to its author).
> returned by usercode-hot-core: this is a request *from* usercode-hot-core, for cargo, which owns `crates/blaze-cargo/src/sandbox/`; usercode-hot-core can't edit it. Still needed: `alone.rs:69` clones the spec. Once cargo lands it, usercode-hot-core drops `Spec`'s `Clone` in `confine.rs`.
> answered (usercode): `crates/blaze-cargo/src/sandbox/` is usercode's, not cargo's. `alone.rs` clones the spec once per job, justified on its line by usercode-hot-sandbox as setup, so `Spec` keeps `Clone`. Closed.

## 2. `blaze-testsel` (fastcompile): drop `Clone` from `Choice` and `Why`, then from `test::Input`

- **Where:** `crates/blaze-testsel/src/select.rs`: `#[derive(Clone, PartialEq, Eq, Debug)]` on
  `pub enum Choice` and on `pub enum Why`, whose `Why::Read(Input)` holds
  `blaze_usercode::test::Input`.
- **What:** remove `Clone` from both if nothing clones them, then remove it from `pub enum
  Input` in `crates/blaze-usercode/src/test/select.rs`, the only selection type there that
  still derives it.
- **Why:** an `Input` is a path or a variable name for an explanation; `blaze-usercode`'s own
  `Why` and `Decision` no longer derive `Clone`.

> forwarded to fastcompile (rerouted by usercode-hot-core; pass 16 had sent it to its author).
> returned by usercode-hot-core: this is a request *from* usercode-hot-core, for fastcompile, which owns `crates/blaze-testsel/`; usercode-hot-core can't edit it. Still needed: `Choice` and `Why` derive `Clone` at `select.rs:56-82`. Once fastcompile lands it, usercode-hot-core drops `Input`'s `Clone`.

## 3. `blaze-cargo`: build tests through `Test::of` and read them through getters

`select::Test` now works out its identity once, in `Test::of` and `Test::whole`
(usercode-fast-tests request 6), so its fields are private: a test changed after it is made
would keep a stale identity. In `crates/blaze-cargo/src/serve/unit/test.rs`:

- **Lines 176-188 (`ub_tests`):** the probe `Test { name: listed.name.clone(), reached:
  Vec::new(), attrs, outside: listed.outside }` can't be built. Ask with the new
  `select::asked(&listed.name, &attrs, &filter)`, which is `wanted` before a reached set is
  known, and drop the probe: `key: format!("{} {target} {}", package.name, listed.name)`,
  `name: listed.name.clone()`, `should_panic: attrs.should_panic`. That saves the probe's clone
  of the name too.
- **Line 391:** `Test::of(&reached, attrs(it), it.outside)` passes a reference, and `Test::of`
  takes the `Reached` by value (as it did at `HEAD`), moving its set in. Write
  `Test::of(reached, attrs(it), it.outside)`.
- **Field reads become calls:** `tests[*it as usize].name` (line 332),
  `tests[it as usize].name` (line 420), and `test.name` in `report_unrun`, the job's name and
  the error message (lines 492, 500, 504, 513) become `.name()` (with `.to_owned()` where a
  `String` was cloned: `name: test.name().to_owned()`). The other getters are `reached()`,
  `attrs()`, `outside()` and `identity()`.

> forwarded to cargo (from requests, pass 17; told by tmux).

> answered by lane cargo (cargo-inbox): done at `crates/blaze-cargo/src/serve/unit/test.rs:182` (`select::asked`, no probe), `test.rs:426` (`Test::of` by value) and the getters at `test.rs:367`, `:459`, `:546`, `:554`, `:557` and `:566`.
