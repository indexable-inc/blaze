# Stabilize and model Polonius Alpha

| Metadata         |                                    |
|:-----------------|------------------------------------|
| Contact | @lqd                               |
| Status           | Accepted                           |
| Tracking issue   | [rust-lang/goals#118] |
| Zulip channel    | [#t-types/polonius][channel]       |
| [types] champion | @jackh726                          |
| Roadmap          | The Borrow Checker Within           |
| What and why     | Stabilize an improved borrow checker that accepts conditional borrows and lending iterators |
| Highlight        | Polonius                            |

[channel]: https://rust-lang.zulipchat.com/#narrow/channel/186049-t-types.2Fpolonius

## Summary

Stabilize the [polonius alpha][alpha] borrow checking analysis, which resolves [common limitations of the borrow checker][pc3] such as the [NLL problem case #3][pc3] and lending iterator patterns ([rust-lang/rust#92985]). This goal covers fixing the remaining known soundness issue, expanding test coverage, building a formal model in [a-mir-formality][] and upstreaming it into the Rust reference, validating performance, and preparing a stabilization report.

[alpha]: https://github.com/rust-lang/rust/pull/143093
[pc3]: https://blog.rust-lang.org/inside-rust/2023/10/06/polonius-update.html#background-on-polonius
[a-mir-formality]: https://github.com/rust-lang/a-mir-formality/

## Motivation

### The status quo

Polonius is an improved version of the borrow checker that resolves common limitations and which is needed to support future patterns such as lending iterators. Over the past three goal periods ([2025h1](https://goals.rust-lang.org/2025h1/Polonius.html), [2025h2](https://goals.rust-lang.org/2025h2/polonius.html)), we have:

* Identified an actionable subset of the full polonius analysis — the "alpha" version — that handles the most impactful cases while scaling well
* Implemented and landed a [functional prototype][alpha] on nightly that passes perf runs and crater runs
* Significantly reduced overhead through a [lazy constraint graph rewrite](https://github.com/rust-lang/rust/pull/150551)
* Identified the remaining phases for gradually improving precision in the future

The alpha analysis is now at the point where stabilization is the natural next step. There is one known soundness issue remaining (related to dead regions outlived by opaque types), and we need to expand testing, validate performance on real-world code, and prepare the documentation and stabilization report.

#### What the alpha analysis accepts

The majority of open issues marked NLL-deferred and fixed-by-polonius are fixed by the alpha analysis. The most impactful example is the "NLL problem case 3" and variations:

```rust
use std::collections::HashMap;
use std::hash::Hash;

fn get_or_insert_default<'r, K: Hash + Eq + Copy, V: Default>(
    map: &'r mut HashMap<K, V>,
    key: K,
) -> &'r mut V {
    match map.get_mut(&key) {
        Some(value) => value,
        None => {
            map.insert(key, V::default());
            map.get_mut(&key).unwrap()
        }
    }
}
```

A similar variation is the filtering lending iterator pattern, where `next()` reborrows `self` in a loop — code that NLLs incorrectly rejects today. Beyond being an ergonomic hazard, this limitation is also an expressiveness gap: it blocks the ability to write [lending iterator patterns](https://github.com/rust-lang/rust/issues/92985) entirely.

#### What the alpha analysis does not accept

Some cases that require full flow-sensitivity are left for future improvements and show the same imprecision as NLLs today. For example, certain patterns of linked-list traversal with conditional reborrowing. These cases are encountered more rarely than the new cases the alpha accepts.

### What we propose to do about it

We propose to stabilize the polonius alpha analysis. This involves:

1. **Fix the remaining known soundness issue** related to liveness of captured regions for opaque types.
2. **Ship a nightly preview** behind a feature gate with a call for testing, to surface any unknown issues.
3. **Expand testing and validation**, including adding tests from open fixed-by-polonius issues, enabling polonius testing on CI, and using [a-mir-formality][] as an oracle to validate the rustc implementation.
4. **Build a formal model and specification** of the borrow checking analysis in a-mir-formality, and upstream it into the Rust reference. This ties into the [experimental language specification](./experimental-language-specification.md) goal as a case study for integrating formal models into the specification process.
5. **Validate performance** on real-world code. We are willing to accept a performance cost in the range of 10–20% for the benefits polonius provides. We are aware of worst-case scenarios that could be larger, but do not yet know whether these occur in practice — the preview period will help answer this.
6. **Prepare for stabilization** with a stabilization report, rustc dev guide documentation, and a blog post announcing the preview.

During the preview period, we will also evaluate and address diagnostics quality for the newly accepted patterns and for cases near the boundary of what the alpha accepts.

### Work items over the next year

#### Stabilize polonius alpha

Fix remaining issues, validate on real-world code, and ship a stable improved borrow checker.

| Task | Owner(s) | Notes |
| ---- | -------- | ----- |
| Fix soundness issue with opaque types and dead regions | @lqd, @tiif | @tiif's [rust-lang/trait-system-refactor-initiative#159] is a pre-requisite for a borrowck fix |
| Expand test coverage | @lqd | We've started doing this during the previous goal period, e.g. from open fixed-by-polonius issues in [rust-lang/rust#145053] |
| Enable polonius testing on CI | @lqd | |
| Ship nightly preview behind feature gate | @lqd | With blog post / call for testing |
| Validate performance on real-world code | @lqd, @amandasystems | |
| Address diagnostics feedback from preview | @lqd | |
| Write stabilization report | @lqd | |
| Write rustc dev guide documentation | @lqd, @amandasystems | |

#### Extend formality for Polonius alpha

Build a formal model of borrow checking in a-mir-formality and upstream it into the Rust reference.

| Task | Owner(s) | Notes |
| ---- | -------- | ----- |
| Build formal model in a-mir-formality | @tiif, @lqd, @nikomatsakis | Validate against rustc implementation |
| Upstream borrow checking specification into Rust reference | @tiif, @lqd | Ties to [experimental language specification](./experimental-language-specification.md) goal |

### The "shiny future" we are working towards

Stable support for the polonius alpha analysis, followed by gradually improving expressiveness. Beyond the alpha, there are further phases of the analysis that can handle more exotic borrowing patterns (such as full flow-sensitivity for linked-list traversals). These future improvements can build on the stable alpha foundation and on the formal model developed as part of this goal.

## Design axioms

* **Don't let perfect be the enemy of good.** The alpha analysis doesn't handle every case the full polonius model could, but it handles the most common and impactful cases. Shipping this subset sooner is better than waiting for a complete solution.

* **Prove it formally.** Building a formal model in a-mir-formality and using it as an oracle gives us confidence that the implementation matches the intended semantics, and produces a specification that lives on as documentation.

* **Accept bounded cost.** We are willing to accept a compile-time overhead of 10–20% for the expressiveness gains polonius provides. Unbounded cost is not acceptable, but modest cost is a reasonable trade-off.

## Team asks

| Team       | Support level | Notes                                                                            |
|------------|---------------|----------------------------------------------------------------------------------|
| [types]    | Large         | Design review, stabilization decision, reviews from @jackh726 and @matthewjasper |

### Support needed from the project

We expect most support to be needed from the types team, for design review, reviews, and the stabilization decision. We expect @nikomatsakis to provide design guidance, and @jackh726 and @matthewjasper to help with reviews.

## Frequently asked questions

### What is the polonius alpha analysis?

The alpha analysis is a location-sensitive version of the borrow checker that uses reachability within a combined subset + CFG graph to determine loan liveness. It is a superset of the current NLL analysis — it accepts everything NLLs accept, plus additional patterns like NLL problem case 3 and lending iterator filtering. It does not handle full flow-sensitivity (e.g., certain linked-list traversal patterns with conditional reborrowing).

### How does this relate to the original datalog-based polonius?

The original datalog implementation could handle more exotic patterns but did not scale and had no path to stabilization. The alpha analysis is a pragmatic subset that captures the most impactful improvements while being practical to ship.

### What happens if the preview reveals unacceptable performance costs?

We have identified specific optimization opportunities (limiting propagation to affected blocks, unifying invariant lifetimes, activation-only invalidations) that we can pursue if needed. Performance work can also be done by others, both before and after stabilization.
