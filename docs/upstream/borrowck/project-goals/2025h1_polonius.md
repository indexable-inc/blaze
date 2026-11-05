# Scalable Polonius support on nightly

| Metadata         |                                    |
|:-----------------|------------------------------------|
| Contact | @lqd                               |
| Status           | Accepted                           |
| Tracking issue   | [rust-lang/goals#118] |
| Zulip channel    | [#t-types/polonius][channel]       |
[channel]: https://rust-lang.zulipchat.com/#narrow/channel/186049-t-types.2Fpolonius
## Summary

Keep working on implementing a native rustc version of the [Polonius][pc3] next generation borrow checking algorithm, that would scale better than the previous [datalog] implementation, continuing from the [2024h2 goal](https://goals.rust-lang.org/2024h2/Polonius.html).

[datalog]: https://github.com/rust-lang/polonius

## Motivation

Polonius is an improved version of the borrow checker that [resolves common limitations of the borrow checker][pc3] and which is needed to support future patterns such as "lending iterators" (see [#92985]). Its model also prepares us for further improvements in the future.

Some support exists on nightly, but this [older prototype][datalog] has no path to stabilization due to scalability issues. We need an improved architecture and implementation to fix these issues.

[pc3]: https://blog.rust-lang.org/inside-rust/2023/10/06/polonius-update.html#background-on-polonius

### The next six months

* Complete the ongoing work to land polonius on nightly

### The "shiny future" we are working towards

Stable support for Polonius.

## Ownership and team asks

**Owner:** lqd

Other support provided by @amandasystems as part of her PhD.

[amanda]: https://github.com/amandasystems

| Task             | Owner(s) or team(s)  | Notes          |
| ---------------- | -------------------- | -------------- |
| Design review    | @nikomatsakis        |                |
| Implementation   | @lqd, @amandasystems |                |
| Standard reviews | ![Team][] [types]    | @matthewjasper |

### Support needed from the project

We expect most support to be needed from the types team, for design, reviews, interactions with the trait solver, and so on. We expect @nikomatsakis, leading the polonius working group and design, to provide guidance and design time, and @compiler-errors and @matthewjasper to help with reviews.

## Outputs and milestones

### Outputs

Nightly implementation of polonius that passes [NLL problem case #3][pc3] and accepts lending iterators ([#92985]).

Performance should be reasonable enough that we can run the full test suite, do crater runs, and test it on CI, without significant slowdowns. We do not expect to be production-ready yet by then, and therefore the implementation would still be gated under a nightly -Z feature flag.

As our model is a superset of NLLs, we expect little to no diagnostics regressions, but improvements would probably still be needed for the new errors.

### Milestones

Note: some of these are currently being worked on as part of the 2024h2 goal, and could be completed before the 2025h1 period.

| Milestone                                                                          | Contributor    | Notes |
| ---------------------------------------------------------------------------------- | -------------- | ----- |
| Factoring out higher-ranked concerns from the main path                            | @amandasystems |       |
| ↳ [x] rewrite invalid universe constraints with outlives `'static` constraints     |                | [PR 123720](https://github.com/rust-lang/rust/pull/123720) |
| ↳ [ ] completely remove placeholders                                               |                | in progress [PR 130227](https://github.com/rust-lang/rust/pull/130227) |
| Location-sensitive prototype on nightly                                            | @lqd           | in progress |
| ↳ [x] create structures for location-dependent outlives constraints                |                |             |
| ↳ [x] build new constraint graph from typeck constraints and liveness constraints  |                |             |
| ↳ [x] update NLLs for required changes to local & region liveness, loan liveness & loan scopes, (possibly unreachable) kills, bidirectional traversal & active loans | | |
| ↳ [ ] limit regressions about diagnostics when using the new constraints on diagnostics tailored to the old constraints  | | |
| ↳ [ ] land on nightly under a `-Z`  flag                                           |                     |             |
| [x] Debugging / dump tool for analysis of location-sensitive analysis              | @lqd                |             |
| [ ] Tests and validation of location-sensitive Polonius                            | @lqd                |             |
| ↳ [ ] make the full test suite pass                                                |                     | in progress |
| ↳ [ ] do a crater run for assertions and backwards-compatibility                   |                     |             |
| ↳ [ ] expand test suite with tests about the new capabilities                      |                     |             |
| [ ] Location-sensitive pass on nightly, tested on CI                               | @lqd                |             |
