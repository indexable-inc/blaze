# Stabilizable Polonius support on nightly

| Metadata         |                                    |
| :--------------- | ---------------------------------- |
| Contact | @lqd                               |
| Status           | Proposed                           |
| Flagship         | Unblocking dormant traits          |
| Tracking issue   | [rust-lang/goals#118] |
| Zulip channel    | [#t-types/polonius][channel]       |
| [types] champion | @jackh726 |
[channel]: https://rust-lang.zulipchat.com/#narrow/channel/186049-t-types.2Fpolonius
## Summary

Make a stabilizable version of the [polonius][pc3] next generation borrow checking "alpha" algorithm. This [revision of the analysis][alpha], while less powerful than we hoped, currently scales better than the previous [datalog] implementation, and accepts the main problem case we deferred from NLLs: it handles almost all our in-tree tests, passes perf runs (but is still too slow) and crater runs without issues. It's therefore a worthwhile step to ship to users, but needs more work to be properly usable on nightly.

This goal is a continuation of the [2025h1 goal](https://goals.rust-lang.org/2025h1/Polonius.html).

[datalog]: https://github.com/rust-lang/polonius
[alpha]: https://github.com/rust-lang/rust/pull/143093

## Motivation

Polonius is an improved version of the borrow checker that [resolves common limitations of the borrow checker][pc3] and which is needed to support future patterns such as "lending iterators" (see [#92985]). Its model also prepares us for further improvements in the future.

In the previous goal periods, we have landed prototypes of the analysis on nightly, and have a [functional version][alpha] that we think is worthwhile to stabilize, even if it's not the full version handling all issues related to flow-sensitivity during borrow-checking.

The key achievements from our past work are:
* identifying this actionable subset
* implementing and evaluating a functional prototype
* identifying other phases we wish to explore later, to gradually improve the precision of the analysis

[pc3]: https://blog.rust-lang.org/inside-rust/2023/10/06/polonius-update.html#background-on-polonius
[#92985]: https://github.com/rust-lang/rust/issues/92985

The design for the polonius $\alpha$ analysis (modulo SCCs) can be summarized as:

* with the CFG $\mathcal{C}$, and the subset graph
* compute a "unified graph" $\mathcal{U}$ where
    * nodes are "region $r$ at point $p$"
    * add outlives edges from type-checking: subset edges between regions $r_0 \subseteq r_1$, at the point $p$ where the constraint occurs
    * add liveness edges: if there is an edge $p\rightarrow q \in \mathcal{C}$ and the region $r_1$ is live at $q$
        * make an edge $r_1$ at $p$ to $r_1$ at $q$
        * respecting variance (forward, backward, or bidirectional)
* "liveness" of a variable is from standard compiler analysis
    * with subtleties about "use" liveness and "drop" liveness
* "liveness" of a region $r$ at point $p$ is:
    * $r$ is in the type of a variable $v$ live at $p$
* "liveness" of a loan $l$ at point $p$ is:
    * there exists a live region $r$ where the loan $l$ can reach $r$ in the "unified graph" $\mathcal{U}$ from its introduction region/point node.
* then we do the usual loans-in-scope computation where a
    * GEN is the loan being introduced
    * KILL is the loan stops being live and/or the place is overwritten
* and using these loans in scope when checking for place invalidations

The alpha version of the analysis uses reachability within the subset+cfg graph to approximate liveness, and uses the same loan-in-scope computation to handle kills as NLLs.

#### Examples that the alpha analysis accepts

The majority of open issues marked NLL-deferred, and fixed-by-polonius, would be fixed: their MCVEs are now being accepted by the alpha analysis.

The most impactful example is the "NLL problem case 3" and variations of it, that were deferred from the NLL implementation.

```rust
use std::collections::HashMap;
use std::hash::Hash;

fn from_the_nll_rfc<'r, K: Hash + Eq + Copy, V: Default>(
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

A similar variation is the filtering lending iterator.

```rust
trait LendingIterator {
    type Item<'a>
    where
        Self: 'a;
    fn next(&mut self) -> Option<Self::Item<'_>>;

    fn filter<P>(self, predicate: P) -> Filter<Self, P>
    where
        Self: Sized,
        P: FnMut(&Self::Item<'_>) -> bool,
    {
        Filter { iter: self, predicate }
    }
}

pub struct Filter<I, P> {
    iter: I,
    predicate: P,
}
impl<I: LendingIterator, P> LendingIterator for Filter<I, P>
where
    P: FnMut(&I::Item<'_>) -> bool,
{
    type Item<'a>
        = I::Item<'a>
    where
        Self: 'a;

    // This is now accepted
    fn next(&mut self) -> Option<I::Item<'_>> {
        while let Some(item) = self.iter.next() {
            if (self.predicate)(&item) {
                return Some(item);
            }
        }
        return None;
    }
}
```

(Note that this doesn't make lending iterators themselves easier to use, due to the unrelated limitations in GATs themselves.)

#### Examples that the alpha analysis does not accept

Some cases that require full flow-sensitivity are left for future improvements, and show the same imprecision as NLLs today. For example, some patterns of linked-list traversals with a cursor. There are a handful of open issues like these.

```rust
struct X {
    next: Option<Box<X>>,
}
```

This is showing the same imprecision as NLLs discussed above:

```rust
fn conditional() {
    let mut b = Some(Box::new(X { next: None }));
    let mut p = &mut b;
    while let Some(now) = p {
        // ^ ERROR: cannot use `*p` because it was mutably borrowed
        if true {
            p = &mut now.next;
        }
    }
}
```

While this is still accepted:

```rust
fn no_control_flow() {
    let mut b = Some(Box::new(X { next: None }));
    let mut p = &mut b;
    while let Some(now) = p {
        p = &mut now.next;
    }
}

fn conditional_with_indirection() {
    let mut b = Some(Box::new(X { next: None }));
    let mut p = &mut b;
    while let Some(now) = p {
        if true {
            p = &mut p.as_mut().unwrap().next;
        }
    }
}
```

Similarly,

```rust
struct Node<T> {
    value: T,
    next: Option<Box<Self>>,
}

type List<T> = Option<Box<Node<T>>>;

fn remove_last_node_recursive<T>(node_ref: &mut List<T>) {
    let next_ref = &mut node_ref.as_mut().unwrap().next;

    if next_ref.is_some() {
        remove_last_node_recursive(next_ref);
    } else {
        *node_ref = None;
    }
}

fn remove_last_node_iterative<T>(mut node_ref: &mut List<T>) {
    loop {
        let next_ref = &mut node_ref.as_mut().unwrap().next;

        if next_ref.is_some() {
            node_ref = next_ref;
        } else {
            break;
        }
    }

    *node_ref = None;
    // ^ ERROR cannot assign to `*node_ref` because it is borrowed
}
```

Or another pattern requiring full flow-sensitivity like the following

```rust
use std::cell::Cell;

struct Invariant<'l>(Cell<&'l ()>);

fn create_invariant<'l>() -> Invariant<'l> {
    Invariant(Cell::new(&()))
}

// Fails
fn use_it<'a, 'b>(choice: bool) -> Result<Invariant<'a>, Invariant<'b>> {
    let returned_value = create_invariant();
    if choice { Ok(returned_value) } else { Err(returned_value) }
}

// OK
fn use_it2<'a: 'b, 'b: 'a>(choice: bool) -> Result<Invariant<'a>, Invariant<'b>> {
    let returned_value = create_invariant();
    if choice { Ok(returned_value) } else { Err(returned_value) }
}
```

### The next six months

* Complete the remaining features we need to support, like member constraints, and fix the last couple diagnostics differences with NLLs
* Keep improving testing and validation
    * continue to expand our test coverage
    * validate and triage open issues marked as NLL-deferred and fixed-by-polonius for the alpha-version of the analysis
    * hopefully compare behavior with future formal works, e.g. in [`a-mir-formality`](https://github.com/rust-lang/a-mir-formality/)
    * maybe identify interesting benchmarks or stress tests
* Refactor, or rewrite, the implementation to reduce overhead, for example by:
    * doing reachability analysis over the CFG + subset graph separately, versus building an entirely new graph like we do today
    * making the algorithm incremental, or lazy, so that more complicated walks only do a subset of the entire graph and incrementally build the same data for every place that is invalidated
    * reworking the analysis to use the region graph SCCs, which is also needed to support member constraints
* Make that overhead visible in fewer cases
    * either by making NLLs itself more lazy, and reducing its own overhead,
    * or computing "possible errors" cheaply,
    * or having a good way to switch from one analysis to the other, e.g. only do the more expensive analysis if there's an NLL error (like the NLL migrate mode was used to transition away from AST borrowck)

### The "shiny future" we are working towards

Stable support for the polonius alpha analysis, before gradually improving expressiveness, by capitalizing on the insights gathered during the previous goal periods.

## Design axioms

The older datalog implementation can accept more exotic borrowing patterns, as it (slowly) elaborates all the data needed to handle full flow-sensitivity, but it also doesn't scale, has no path to stabilization and suffers from other shortcomings. In order to not let "perfect be the enemy of good", we've chosen to reduce the scope to a manageable subset that we can ship sooner rather than later.

NLL problem case 3, and the like, are a common issue encountered by users, and we believe handling these kinds of patterns is worthwhile, without needing to wait for a solution to handle even more cases. Especially as we think these cases are encountered more rarely than the new cases we'll accept.

## Ownership and team asks

**Owner:** lqd

Other support provided by @amandasystems as part of her PhD.

| Task             | Owner(s) or team(s)  | Notes                     |
| ---------------- | -------------------- | ------------------------- |
| Design review    | @nikomatsakis        |                           |
| Implementation   | @lqd, @amandasystems |                           |
| Standard reviews | ![Team][] [types]    | @jackh726, @matthewjasper |

### Support needed from the project

We expect most support to be needed from the types team, for design, reviews, interactions with the trait solver, and so on. We expect @nikomatsakis, leading the polonius working group and design, to provide guidance and design time, and @jackh726 and @matthewjasper to help with reviews.

## Outputs and milestones

### Outputs

Nightly implementation of polonius that passes [NLL problem case #3][pc3] and accepts lending iterators ([#92985]).

This implementation should be good enough to be stabilizable, both in features and performance, should pass the full test suite, do crater runs, and test it on CI.

As our model is a superset of NLLs, we expect little to no diagnostics regressions, but improvements would probably still be needed for the new errors.

### Milestones

Note: some of these are currently being worked on and close to being done, and could be completed before the 2025h2 period.

| Milestone                                                                                                                                                            | Owner          | Notes                                                                    |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------- | ------------------------------------------------------------------------ |
| Factoring out higher-ranked concerns from the main path                                                                                                              | @amandasystems |                                                                          |
| ↳ [x] rewrite invalid universe constraints with outlives `'static` constraints                                                                                       |                |                                                                          |
| ↳ [ ] completely remove placeholders                                                                                                                                 |                | in progress, PR [#130227](https://github.com/rust-lang/rust/pull/130227) |
| Location-sensitive prototype on nightly                                                                                                                              | @lqd           |                                                                          |
| ↳ [x] create structures for location-dependent outlives constraints                                                                                                  |                |                                                                          |
| ↳ [x] build new constraint graph from typeck constraints and liveness constraints                                                                                    |                |                                                                          |
| ↳ [x] update NLLs for required changes to local & region liveness, loan liveness & loan scopes, (possibly unreachable) kills, bidirectional traversal & active loans |                |                                                                          |
| ↳ [x] limit regressions about diagnostics when using the new constraints on diagnostics tailored to the old constraints                                              |                |                                                                          |
| ↳ [x] land on nightly                                                                                                                                                |                |                                                                          |
| [x] Debugging / dump tool for analysis of location-sensitive analysis                                                                                                | @lqd           |                                                                          |
| Expand prototype into alpha version                                                                                                                                  | @lqd           |                                                                          |
| ↳ [ ] Handle member constraints, and SCCs                                                                                                                            |                |                                                                          |
| ↳ [ ] Reduce overhead of the analysis                                                                                                                                |                |                                                                          |
| ↳ [ ] Make the analysis incremental and/or lazy                                                                                                                      |                |                                                                          |
| Tests and validation                                                                                                                                                 | @lqd           | in progress                                                              |
| ↳ [ ] make the full test suite pass                                                                                                                                  |                | in progress, PR [#143093]                                                |
| ↳ [x] do a crater run for assertions and backwards-compatibility                                                                                                     |                |                                                                          |
| ↳ [ ] expand test suite with tests about the new capabilities                                                                                                        |                | in progress, PR [#143093](https://github.com/rust-lang/rust/pull/143093) |
| [ ] Alpha version on nightly, tested on CI                                                                                                                           | @lqd           |                                                                          |
