# rust-lang/rust and rust-lang/compiler-team items about Polonius alpha (excerpts)

Fetched 2026-10-20 through the GitHub REST API (`https://api.github.com/repos/<repo>/issues/<n>` and `/pulls/<n>`).
Bodies are quoted as posted (truncated where marked); each heading links to the item.

---

## [rust-lang/rust#159343](https://github.com/rust-lang/rust/pull/159343): Enable polonius alpha on nightly

- PR, state `closed`, opened 2026-07-15 by @jackh726, merged 2026-08-05
- labels: T-compiler, T-bootstrap, merged-by-bors, perf-regression

Body:

> <!-- homu-ignore:start -->
> *[View all comments](https://triagebot.infra.rust-lang.org/gh-comments/rust-lang/rust/pull/159343)*
> <!-- homu-ignore:end -->
> 
>  See [rust-lang/compiler-team#](https://github.com/rust-lang/compiler-team/issues/1015)
> 
> The first commit here adds an `-Zpolonius=nll` argument for tests and so people can disable alpha on nightly.

---

## [rust-lang/compiler-team#1015](https://github.com/rust-lang/compiler-team/issues/1015): Enable `-Zpolonius=next` on nightly

- issue, state `closed`, opened 2026-07-15 by @jackh726
- labels: T-compiler, major-change, major-change-accepted

Body:

> # Proposal
> 
> We want to enable Polonius Alpha (under -Zpolonius=next) on nightly. It has no known regressions, no known soundness issues, and allows strictly more sound code to compile:
> - `tests/ui/nll/polonius/filtering-lending-iterator-issue-92985.rs`
> - `tests/ui/nll/polonius/iterating-updating-mutref.rs`
> - `tests/ui/nll/polonius/nll-problem-case-3-issue-*.rs`
> 
> ## Performance
> 
> Latest performance numbers to come, but prior numbers indicate around a ~2% mean regression (due to additional liveness computation) in the perf suite, but analysis of ecosystem indicates there are some crates that experience larger regressions:
> 
> Profiling the full compilation of the top 10k crates reveal 91 crates past an arbitrary threshold of 2%, quadratically for lower compilation times:
> 
> <img width="576" height="432" alt="Image" src="https://github.com/user-attachments/assets/478d7546-3737-4a97-9baf-4d0154c3e64a" />
> 
> Here are the top 5 by ratio:
> 
> <img width="480" height="193" alt="Image" src="https://github.com/user-attachments/assets/5bf954c4-9175-4b3d-82e4-3b1f8b3582c8" />
> 
> Profiling just the *leaf* crate compilation time of the top 5k crates reveal 61 crates past a similar scaling threshold of 1%:
> 
> <img width="583" height="438" alt="Image" src="https://github.com/user-attachments/assets/e927e9cd-3c85-4bcf-b2c2-dadf1a0b76dd" />
> 
> <img width="660" height="183" alt="Image" src="https://github.com/user-attachments/assets/72e0ff93-4967-4b4a-8d4f-0504c5957961" />
> 
> We've initially triaged some of these and plan to work on mitigations (general or targeted) prior to stabilization.
> 
> We've done a bit of limited triage of crates outside the top 10k, and we do know that there are cases that have worse than the ~20% regression in the top 10k crates. For example, this build.rs has a 2-3x regression: https://github.com/ernestas-poskus/tld.rs/blob/master/build.rs.
> 
> We'll similarly triage things we find and attempt mitigations.
> 
> ## Testing plan
> 
> Given that we expect generally no regressions (bugs or *major* performance), we would like to enable Polonius Alpha on nightly *and keep it enabled until stabilization*. This gives us a long ramp for people to discuss issues if the exist (since we expect they will be harder to discover than something like trait solving bugs).
> 
> If we want to just change the default for the `-Zpolonius` option, we can either:
> 1) Juggle a nightly disable + re-enable on either side of the release cutoff.
> 2) Include the disable into the beta branch.
> 
> Alternatively, we can just detect nightly in `is_next_enabled`.
> 
> Of course, if any issues come up, we plan to disable on nightly until fixes.
> 
> # Process
> 
> The main points of the [Major Change Process][MCP] are as follows:
> 
> * [x] File an issue describing the proposal.
> * [ ] A compiler team member who is knowledgeable in the area can **second** by writing `@rustbot second` or kickoff a team FCP with `@rfcbot fcp $RESOLUTION`.
>     * Refer to [Proposals, Approvals and Stabilization](https://forge.rust-lang.org/compiler/proposals-and-stabilization.html) docs for when a second is sufficient, or when a full team FCP is required.
> * [ ] Once an MCP is seconded, the **Final Comment Period** begins.
>     * Final Comment Period lasts for 10 days after all outstanding concerns are solved.
>     * Outstanding concerns will block the Final Comment Period from finishing. Once all concerns are resolved, the 10 day countdown is restarted.
>     * If no concerns are raised after 10 days since the resolution of the last outstanding concern, the MCP is considered **approved**.
> 
> You can read [more about Major Change Proposals on forge][MCP].
> 
> [MCP]: https://forge.rust-lang.org/compiler/proposals-and-stabilization.html#how-do-i-submit-an-mcp

---

## [rust-lang/rust#160456](https://github.com/rust-lang/rust/issues/160456): Tracking Issue: Polonius Alpha on nightly

- issue, state `open`, opened 2026-08-03 by @jackh726
- labels: C-tracking-issue

Body:

> This is intended to be a "tracking issue" for anything related to enabling Polonius Alpha on nightly ahead of stabilization.
> 
> If you encounter an issue (a bug, unsoundness, or significant performance regression) related to the borrow checker while Polonius Alpha is enabled, please feel free to post here. Issues requiring longer discussion can also be discussed [on Zulip][zulip].
> 
> **Status**: Enabled on nightly (starting nightly-2026-08-06)
> 
> ---
> 
> Relevant links:
> - Blog post: https://blog.rust-lang.org/2026/08/04/enabling-polonius-alpha-on-nighty/
> - Zulip stream for discussion: https://rust-lang.zulipchat.com/#narrow/channel/186049-t-types.2Fpolonius/topic/Polonius.20Alpha.20enabled.20on.20nightly/near/614373312
> - Enabling PR: https://github.com/rust-lang/rust/pull/159343
> - MCP: https://github.com/rust-lang/compiler-team/issues/1015
> 
> [zulip]: https://rust-lang.zulipchat.com/#narrow/channel/186049-t-types.2Fpolonius

---

## [rust-lang/rust#160588](https://github.com/rust-lang/rust/pull/160588): add a test showing polonius alpha is not a subset of datalog polonius

- PR, state `closed`, opened 2026-08-05 by @lqd, merged 2026-08-06
- labels: T-compiler, T-bootstrap, S-waiting-on-bors

Body:

> This adds a test showing how the datalog and alpha algorithms are not subsets of one another, and are slightly distinct subsets of a platonic ideal borrowck.
> 
> r? @jackh726 

---

## [rust-lang/rust#153215](https://github.com/rust-lang/rust/issues/153215): free region visitor for liveness marking regions dead and polonius alpha soundness

- issue, state `open`, opened 2026-02-28 by @lqd
- labels: A-borrow-checker, C-bug, NLL-polonius, T-types

Body:

> This tracks the work to fix a soundness issue between the free region liveness visitor and polonius (the model in general and the polonius alpha in its current implementation) as seen in UI tests under edition 2024 and `-Zpolonius=next`:
> - `tests/ui/impl-trait/alias-liveness/rpit-hide-lifetime-for-swap.rs`
> - `tests/ui/impl-trait/hidden-lifetimes.rs`
> 
> <sub>Both tests exercise the same issue and codepaths for our purpose here, one of these is actually a superset of the other.</sub>
> 
> The free region visitor for liveness is coarse with respect to unused regions, and can mark only the region that is outlived by the opaque type as live, but not the _regions that outlive it_. This is _fine_ under NLLs, but not with the location-sensitive-liveness-based-analysis artist known as polonius: the polonius alpha propagates loans in the CFG as a traversal, according to liveness and outlives constraints, and encountering _only_ dead regions at a point can cause to miss errors, by definition. The loan will not be marked live, and an illegal access error is an invalidation of a live loan.
> 
> (The old datalog implementation doesn't suffer from this issue by chance: it _reimplements liveness_ and hasn't been updated to try to ignore unused regions captured by an opaque type. Yes, that previous implementation now rejects the stable code relying on this new behavior. Code that is more common with the new edition 2024 capture rules, and `use<>` bounds.)
> 
> This behavior is due to [this branch](https://github.com/rust-lang/rust/blob/846cb1e850ae22b0abf853a4acdfe4b67c07aab1/compiler/rustc_infer/src/infer/outlives/for_liveness.rs#L88-L93), but by now, stable code relies on unused lifetimes being marked dead by it, and we can't go back.
> 
> Our expectation is that fixing [an opaque type unsoundness](https://github.com/rust-lang/trait-system-refactor-initiative/issues/159) with the new solver will create the prerequisites to use in the liveness visitor and fix the current issue, by having a way to be less coarse in the selection of regions to consider unused and mark dead. The draft fix for that issue is https://github.com/rust-lang/rust/pull/152051. 
> 
> This issue is to ensure this doesn't slip through, and mostly to be able to mark these two tests as `known-bug` and be aware when anything changes here.

---

## [rust-lang/rust#161038](https://github.com/rust-lang/rust/issues/161038): Unsoundness in Polonius involving impl Trait

- issue, state `open`, opened 2026-08-13 by @theemathas
- labels: A-borrow-checker, I-unsound, A-impl-trait, C-bug, NLL-polonius, requires-nightly, T-types

Body:

> The following code was created based on #160979 with trial and error. It compiles in nightly and segfaults. It doesn't compile with `-Zpolonius=off`.
> 
> ```rust
> type Payload = Box<i32>;
> 
> trait Produce<'a> {
>     fn produce(self) -> &'a &'a Payload;
> }
> impl<'a> Produce<'a> for &'a &'a Payload {
>     fn produce(self) -> &'a &'a Payload {
>         self
>     }
> }
> 
> trait Outlives<'a, 'b> {
>     fn shorten(x: impl Produce<'a> + 'a) -> impl Produce<'a> + 'b;
> }
> impl<'a: 'b, 'b> Outlives<'a, 'b> for () {
>     fn shorten(x: impl Produce<'a> + 'a) -> impl Produce<'a> + 'b {
>         x
>     }
> }
> 
> fn make_producer<'a, 'b>(payload: &'a &'b Payload) -> impl Produce<'a> + 'b + use<'a, 'b>
> where
>     (): Outlives<'a, 'b>,
> {
>     <()>::shorten(payload)
> }
> 
> fn main() {
>     let wrong: &&Payload;
>     let a = Box::new(1);
>     {
>         let mut r: &Payload = &a;
>         let x: &&Payload = &r;
>         wrong = make_producer(x).produce();
>         r = &*Box::new(Box::new(2));
>     }
>     println!("{wrong}");
> }
> ```
> 
> ### Meta
> 
> Reproducible on the playground with version `1.99.0-nightly (2026-08-12 c98d0cb27cc63afdd626)`

---

## [rust-lang/rust#161471](https://github.com/rust-lang/rust/issues/161471): Polonius soundness bug with higher ranked function pointers

- issue, state `open`, opened 2026-08-21 by @theemathas
- labels: A-borrow-checker, I-unsound, C-bug, NLL-polonius, requires-nightly, T-types, A-higher-ranked

Body:

> I'm filing this issue on behalf of @carlini, who discovered the [initial unsoundness reproducer](https://rust-lang.zulipchat.com/#narrow/channel/186049-t-types.2Fpolonius/topic/Reporting.20a.20few.20Polonius.20bugs/near/617350569) with an LLM. I then minimized the code to the code below.
> 
> The code below creates a `&mut` that aliases with a `&`, and somehow uses both of them, causing unsoundness. The aliasing references are `r` and `p`.
> 
> ```rust
> #![forbid(unsafe_code)]
> 
> type Payload = Option<Box<i32>>;
> 
> type P<'a, 'b> = for<'x> fn(&'a mut &'b Payload, &'x ()) -> &'x Payload;
> 
> fn f<'a, 'b, 'c>(x: &'a mut &'b Payload, _: &'c ()) -> &'b Payload {
>     *x
> }
> 
> fn main() {
>     let p: &'static mut Payload = Box::leak(Box::new(Some(Box::new(1))));
>     let mid: &mut &Payload = Box::leak(Box::new(&*p));
>     let r: &Payload = (f as P<'_, '_>)(mid, &());
>     let b: &Box<i32> = r.as_ref().unwrap();
>     if false {
>         let _: &mut &'static Payload = mid;
>     } else {
>         *p = None;
>         println!("{b}");  // Segfault
>     };
> }
> ```
> 
> The unsoundness reproduces with `-Zpolonius=next`, but doesn't reproduce (fails to compile) with `-Zpolonius=no`.
> 
> ### Meta
> 
> Reproducible on the playground with version `1.100.0-nightly (2026-08-20 8925ea358a0f265ca610)`

---

## [rust-lang/rust#164337](https://github.com/rust-lang/rust/issues/164337): Polonius (nightly default) loses the borrow captured by an `impl Trait` return value when the callee has a projection-typed parameter and is called through a closure

- issue, state `open`, opened 2026-10-15 by @leeewee
- labels: A-closures, regression-from-stable-to-nightly, A-associated-items, A-borrow-checker, I-unsound, A-impl-trait, C-bug, NLL-polonius, I-prioritize, T-types

Body:

> Minimal reproducer (edition 2024, where return-position `impl Trait` captures all lifetimes):
> 
> ```rust
> trait Id { type Out: ?Sized; }
> impl<X: ?Sized> Id for X { type Out = X; }
> 
> fn f<T: std::fmt::Display>(t: <&T as Id>::Out) -> impl std::fmt::Display {
>     t
> }
> 
> fn main() {
>     let mut x = 0;
>     let a = (|| f(&x))();
>     x = 1; // should be rejected: `a` borrows `x`
>     println!("{a}");
> }
> ```
> 
> This compiles on current nightly and has undefined behaviour under Miri (an aliasing violation). It is rejected by NLL (`-Zpolonius=off`: E0506), by 1.100.0-beta.5 and by 1.99.0 stable (edition 2024). Writing the parameter as `&T`, or calling `f(&x)` directly, are both correctly rejected.
> 
> The same happens with an `async fn`, whose return type is an opaque `impl Future`. This is how it was first found:
> 
> ```rust
> use std::future::Future;
> use std::pin::pin;
> use std::task::{Context, Poll, Waker};
> 
> trait Id { type Out: ?Sized; }
> impl<X: ?Sized> Id for X { type Out = X; }
> 
> async fn multiple_named_lifetimes<'a, 'b>(a: <&'a u8 as Id>::Out, _: &u8) -> &'a u8 {
>     a
> }
> 
> fn main() {
>     let mut a = 22u8;
>     let b = 44u8;
>     let future = (|| multiple_named_lifetimes(&a, &b))();
>     a += 1; // should be rejected: `future` still borrows `a`
>     let mut fut = pin!(future);
>     let mut cx = Context::from_waker(Waker::noop());
>     if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
>         println!("{}", *r);
>     }
> }
> ```
> 
> **Expected:** `error[E0506]: cannot assign to `a` because it is borrowed`, as with NLL.
> 
> **Actual:** compiles. Under Miri:
> 
> ```
> error: Undefined Behavior: trying to retag from <505> for SharedReadOnly permission at alloc206[0x0], but that tag does not exist in the borrow stack for this location
>  --> src/main.rs:8:43
>   |
> 8 | async fn multiple_named_lifetimes<'a, 'b>(a: <&'a u8 as Id>::Out, _: &u8) -> &'a u8 {
>   |                                           ^ this error occurs as part of retag at alloc206[0x0..0x1]
> ```
> 
> **Both ingredients are required.** Each of these is correctly rejected:
> - the same program with `a: &'a u8` (no projection);
> - the direct call `multiple_named_lifetimes(&a, &b)` (no closure).
> 
> **Versions / bisection:**
> 
> | Configuration | Result |
> |---|---|
> | nightly-2026-10-13 (`e9bbadf68`), default | accepted |
> | nightly-2026-10-13, `-Zpolonius=off` / `-Zpolonius=legacy` | rejected |
> | nightly-2026-10-13, `-Zpolonius=next` | accepted |
> | nightly-2026-10-13, `-Znext-solver=coherence` and `=globally` | accepted with both trait solvers |
> | `-Zpolonius=next` on nightly-2025-01-01 | rejected |
> | `-Zpolonius=next` on nightly-2025-07-01 and later | accepted |
> | default nightly, 2026-08-05 (`1ed2df61a19`) | rejected |
> | default nightly, 2026-08-06 (`7608eb7b07e`, the merge of #159343 "Enable polonius alpha on nightly") | accepted |
> | 1.100.0-beta.5, 1.99.0 stable | rejected |
> 
> So the bug has been present in Polonius alpha since the first half of 2025. #159343 made it reachable on default nightly.
> 
> **Possibly related:** #160979 ("A possible regression with -Zpolonius=next", A-impl-trait) and #161038 ("Unsoundness in Polonius involving impl Trait"). The `async fn` return type is an opaque `impl Future`, so this may share their root cause. The trigger here (a projection-typed parameter plus a closure call) is different.
> 
> **LLM disclosure**
> The bug was found by an automated testing tool developed with the assistance of an LLM (Claude). This report text was drafted by an LLM and reviewed/edited by me.
> 
> ### Meta
> `rustc --version --verbose`:
> ```
> rustc 1.101.0-nightly (e9bbadf68 2026-10-12)
> commit-hash: e9bbadf6824e65b3583c2dda3a0926ec76df9143
> ```

---

## [rust-lang/rust#164420](https://github.com/rust-lang/rust/issues/164420): Polonius (old trait solver): fix for #160669 is skipped once the body contains a higher-ranked type

- issue, state `open`, opened 2026-10-16 by @leeewee
- labels: P-low, regression-from-stable-to-nightly, A-borrow-checker, I-unsound, A-impl-trait, C-bug, NLL-polonius, T-types, fixed-by-next-solver, A-higher-ranked

Body:

> The code below is #160669's reproducer with one added line, `let _f: fn(&u8) = g;`. #161151 fixed #160669, but with this line the program compiles again (Polonius alpha, old trait solver), and Miri reports a use-after-free.
> 
> ```rust
> #![forbid(unsafe_code)]
> use std::fmt::Display;
> 
> fn g(_: &u8) {}
> 
> fn foo<'a>(s: &'a String, flag: bool) -> impl Display + use<'a> {
>     let _f: fn(&u8) = g; // the only change to #160669's program (plus `fn g`)
>     if flag {
>         let local = String::from("dangling");
>         return &local;
>     }
>     s
> }
> 
> fn main() {
>     let s = String::from("ok");
>     let r = foo(&s, true);
>     println!("{}", r); // reads freed heap + stack memory
> }
> ```
> 
> ### I expected to see this happen
> 
> E0597 (`local` does not live long enough), as for #160669's program, which is the same code without the two added lines.
> 
> ### Instead, this happened
> 
> It compiles with `-Znext-solver=coherence` on current nightly (Polonius alpha is the nightly default).
> 
> ### Versions and configurations
> 
> - **nightly-2026-08-19 and 2026-08-20:** these are after #161151 merged (2026-08-18), while the nightly default was still Polonius alpha with the old trait solver. With **default flags**, #160669's program is rejected and this program is accepted. So the fix was incomplete in the default configuration when it landed.
> - **nightly-2026-10-15:**
>   - accepted with `-Znext-solver=coherence`;
>   - rejected with default flags: the next solver has been the nightly default since #160619 on 08-21;
>   - rejected with `-Zpolonius=off` (NLL).
> 
> **LLM disclosure:** The bug was found by an automated testing tool developed with the assistance of an LLM. This report text was drafted by an LLM and reviewed/edited by me.
> 
> ### Meta
> `rustc --version --verbose`:
> ```
> rustc 1.101.0-nightly (7df81381d 2026-10-14)
> binary: rustc
> commit-hash: 7df81381d027710708209f36b98d11b1c88f8d2e
> commit-date: 2026-10-14
> host: x86_64-unknown-linux-gnu
> release: 1.101.0-nightly
> LLVM version: 23.1.3
> ```

---

## [rust-lang/rust#164421](https://github.com/rust-lang/rust/issues/164421): Borrow held by an `impl Trait` value is lost when the value is passed through a closure (Polonius; the fn-pointer form also with NLL)

- issue, state `open`, opened 2026-10-16 by @leeewee
- labels: A-closures, A-borrow-checker, regression-from-stable-to-stable, I-unsound, A-impl-trait, C-bug, I-prioritize, T-types

Body:

> In the following code:
> 
> ```rust
> fn get<'a>(mut items: impl Iterator<Item = &'a u64>) -> impl Fn() -> u64 + 'a {
>     let r = items.next().unwrap();
>     move || *r
> }
> fn vid<T>(t: T) -> T { t }
> fn main() {
>     let mut v = vec![1u64];
>     let read = ({
>         let f: Box<dyn FnOnce(_) -> _> = Box::new(|a| vid(a));
>         f
>     })(get(v.iter()));
>     v.clear();
>     v.shrink_to_fit();
>     v.extend([2u64; 64]);
>     println!("{}", read());
> }
> ```
> 
> **I expected to see this happen:** An error at `v.clear()` (`v` is still borrowed by `read`), as with `-Zpolonius=off`: E0502.
> 
> **Instead, this happened:** It compiles on nightly-2026-10-15 with default flags (Polonius alpha), under both trait solvers. 
> 
> 
> ### Second example
> 
> A closely related form is accepted by NLL as well, and therefore on stable since 1.89. In that form the closure is coerced to a fn pointer instead of being boxed, and `get` takes a projection-typed parameter.
> 
> ```rust
> trait Id { type Out: ?Sized; }
> impl<X: ?Sized> Id for X { type Out = X; }
> fn get<'a>(r: <&'a u64 as Id>::Out) -> impl Fn() -> u64 + 'a { move || *r }
> fn main() {
>     let mut v = vec![1u64];
>     let read = ((|a| a) as fn(_) -> _)(get(&v[0]));
>     v.clear(); v.shrink_to_fit(); v.extend([2u64; 64]);
>     println!("{}", read());
> }
> ```
> 
> - **Accepted:** stable 1.89.0, 1.93.0 and 1.99.0; beta 1.100.0-beta.5; nightly-2026-10-15 with default flags and with `-Zpolonius=off`. 
> - **Rejected:** stable 1.88.0 and earlier.
> 
> **Related:** #164337, where the call itself is made inside a closure. That issue's NLL claim ("rejected by NLL") still holds for its own program, but the fn-pointer form above shows that a closely related shape reaches NLL and stable.
> 
> **LLM disclosure:** The bug was found by an automated testing tool developed with the assistance of an LLM. This report text was drafted by an LLM and reviewed/edited by me.
> 
> ### Meta
> `rustc --version --verbose`:
> ```
> rustc 1.101.0-nightly (7df81381d 2026-10-14)
> binary: rustc
> commit-hash: 7df81381d027710708209f36b98d11b1c88f8d2e
> commit-date: 2026-10-14
> host: x86_64-unknown-linux-gnu
> release: 1.101.0-nightly
> LLVM version: 23.1.3
> ```
> Stable: `rustc 1.99.0 (b940084d7 2026-09-28)`.

---

## [rust-lang/rust#161909](https://github.com/rust-lang/rust/pull/161909): Report the configured Polonius default in -Z help

- PR, state `closed`, opened 2026-08-28 by @chenyukang, merged 2026-08-30
- labels: T-compiler, S-waiting-on-bors, A-run-make

Body:

> Fixes rust-lang/rust#161543

---

## [rust-lang/rust#143093](https://github.com/rust-lang/rust/pull/143093): Simplify polonius location-sensitive analysis

- PR, state `closed`, opened 2025-06-27 by @lqd, merged 2025-08-10
- labels: A-testsuite, T-compiler, T-bootstrap, S-waiting-on-bors, A-compiletest

Body:

> This PR reworks the location-sensitive analysis into what we think is a worthwhile subset of the datalog analysis. A sort of polonius alpha analysis that handles NLL problem case 3 and more, but is still using the faster "reachability as an approximation of liveness", as well as the same loans-in-scope computation as NLLs -- and thus doesn't handle full flow-sensitivity like the datalog implementation.
> 
> In the last few months, we've identified this subset as being actionable:
> - we believe we can make a stabilizable version of this analysis
> - it is an improvement over the status quo
> - it can also be modeled in a-mir-formality, or some other formalism, for assurances about soundness, and I believe @nikomatsakis is interested in looking into this during H2.
> - and we've identified the areas of work we wish to explore later to gradually expand the supported cases: the differences between reachability and liveness, support of kills, and considerations of time-traveling, for example.
> 
> The approach in this PR is to try less to have the graph only represent live paths, by checking whether we reach a live region during traversal and recording the loan as live there, instead of equating traversal with liveness like today because it has subtleties with the typeck edges in statements (that could forward loans to the successor point without ensuring their liveness). We can then also simplify these typeck stmt edges. And we also can simplify traversal by removing looking at kills, because that's enough to handle a bunch of NLL problem 3 cases -- and we can gradually support them more and more in traversal in the future, to reduce the approximation of liveness.
> 
> There's still some in-progress pieces of work w/r/t opaque types that I'm expecting [lcnr's opaque types rework](https://github.com/rust-lang/rust/pull/139587), and [amanda's SCCs rework](https://github.com/rust-lang/rust/pull/130227) to handle. That didn't seem to show up in tests until I rebased today (and shows lack of test coverage once again) when https://github.com/rust-lang/rust/pull/142255 introduced a couple of test failures with the new captures rules from edition 2024. It's not unexpected since we know more work is needed with member constraints (and we're not even using SCCs in this prototype yet)
> 
> I'll look into these anyways, both for future work, and checking how these other 2 PRs would change things.
> 
> ---
> 
> I'm not sure the following means a lot until we have some formalism in-place, but:
> - I've changed the polonius compare-mode to use this analysis: the tests pass with it, except 2 cases with minor diagnostics differences, and the 2 edition 2024 opaque types one I mentioned above and need to investigate
> - things that are expected to work still do work: it bootstraps, can run our rustc-perf benchmarks (and the results are not even that bad), and a crater run didn't find any regressions (forgetting that crater currently fails to test around a quarter of all crates 👼)
> - I've added tests with improvements, like the NLL problem case 3 and others, as well as some that behave the same as NLLs today and are thus worse than the datalog implementation
> 
> r? @jackh726 
> 
> (no rush I know you're deep in phd work and "implmentating" the new trait solver for r-a :p <3)
> 
> This also fixes rust-lang/rust#135646, a diagnostics ICE from the previous implementation.

---

## [rust-lang/rust#150551](https://github.com/rust-lang/rust/pull/150551): Compute localized outlives constraints lazily

- PR, state `closed`, opened 2025-12-31 by @lqd, merged 2026-02-13
- labels: T-compiler, S-waiting-on-bors

Body:

> This PR rewrites the loan liveness algorithm to compute localized constraints lazily during traversal, to avoid the sometimes costly conversion and indexing happening in the current eager algorithm (or for now as well, the need to reshape the liveness data to better list regions live at a given point).
> 
> It thus greatly reduces the current alpha overhead, although it wasn't entirely removed of course (we're obviously doing more work to improve precision): the worst offending benchmark has a +5-6% wall-time regression — but icounts are worse looking (+13%) rn.
> 
> Best reviewed per-commit, as steps are in sequence to simplify the process or unify some things.
> 
> r? @jackh726 

---

## [rust-lang/rust-analyzer#23043](https://github.com/rust-lang/rust-analyzer/pull/23043): anti-feat: Remove borrowck

- PR, state `closed`, opened 2026-08-05 by @ChayimFriedman2, merged 2026-08-05

Body:

> [Like discussed on Zulip](https://rust-lang.zulipchat.com/#narrow/channel/185405-t-compiler.2Frust-analyzer/topic/Ripping.20borrowck/with/614692046):
> 
> > Our borrowck is both hopeless and not important
