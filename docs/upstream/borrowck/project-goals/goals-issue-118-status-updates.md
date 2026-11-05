# rust-lang/goals#118 — Polonius project goal status updates (all comments)
Source: https://github.com/rust-lang/goals/issues/118 (fetched via the GitHub REST API, `/repos/rust-lang/goals/issues/118/comments`, 2026-10-20).
Comments are reproduced verbatim (status updates by the goal owners), oldest first.

---

## 2024-07-23T19:00:03Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2246035646>

I can't modify the issue text, but the following is needed to fix the link:

```
[pc3]: https://blog.rust-lang.org/inside-rust/2023/10/06/polonius-update.html#background-on-polonius
```

---

## 2024-07-25T14:08:46Z — @nikomatsakis

<https://github.com/rust-lang/goals/issues/118#issuecomment-2250418371>

This issue is intended for status updates only.

For general questions or comments, please contact the owner(s) directly.

---

## 2024-09-01T11:15:21Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2323293342>

Status update:
- we've completed the first task of removing universe errors out of the main path and moving higher-ranked concerns out of the borrowck, in @amandasystems' [PR #123720](https://github.com/rust-lang/rust/pull/123720). During SCC construction, we now detect these constraints and rewrite them to regular constraints outliving `'static`. We have possible improvements in mind that we can do in the future here, but we've also just started trying to remove placeholders, by replacing them with constraints again.
- we've also made more progress on the location-sensitive analysis, though not as much as we'd like. By now we have a "localized" constraint graph to implement propagation in a flow-sensitive manner, with constraints from typeck and liveness, and have it hooked it up to the existing borrow checking in non-lexical scope and active loans computation. It's still buggy though: there are a couple hundred of UI test failures at the moment, including important cases that don't work yet. Some of these failures are due to the diagnostics subsystem not yet handling flow-sensitive concerns. The next immediate steps are to analyze and fix the test failures, and to help with this we're also expanding the existing NLL logging, MIR dumps, and visualizations.

- and we've also started work on the item to use more of the polonius model in the borrow-checker, though we're still in the preparation phase to have an overview of what we want, need, and can change here. Currently, the gated location-insensitive analysis is mainly implemented in the borrowck dataflow analyses, but we want to expand this to replace code and data existing in both models, avoid duplicate work, and simplify the implementation. For example, we can either change components internals to do so, or have them generic enough to work with both models: this applies at least to region liveness, region values, SCC values, etc. We also want to do this unification while taking the location-sensitive requirements into account, as well as constraints that existing downstream code can't be impacted or changed too much for now (e.g. diagnostics).

---

## 2024-10-14T16:29:32Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2411729060>

Key developments:
- We were able to fix a bunch of test failures due to some off-by-one errors (in both typeck and livenesss constraints) from the old "mid-points" that don't exist anymore.
- We're still debugging the large number of test failures, and continued the design work on the unified debugging visualization to help with that. We're currently manually doing the tracing work, and after a few run-throughs within the team, should be able to automate it.
- We've also focused on some examples that we want to accept but do not yet, in particular all the variations of [issue #47680](https://github.com/rust-lang/rust/issues/47680). Some adjustments to the active loans computation may be needed, and/or the locations where effects happen (which also relate to the mid-points item above), which was unexpected. It shouldn't be hard to fix though.
- Amanda has continued cleaning up placeholders in the work-in-progress [PR #130227](https://github.com/rust-lang/rust/pull/130227).
- There was no particular progress on using more of the polonius model within the borrowck,. We also had a bit less time than usual with me going to RustConf, and Amanda going to EuroRust and doing a [presentation on polonius](https://eurorust.eu/2024/talks/the-first-six-years-in-the-development-of-polonius/) 🎉. We'll add a link to the recording once they're made available.

---

## 2024-11-15T22:40:30Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2480065602>

Key developments, with good progress:
- I've completed the loan liveness + active loans changes I mentioned last time, with kills stopping traversal in the new localized constraint graph 
- issues with missing liveness edges when regions flowed into universal regions were fixed
- issues with liveness optimizations for NLLs, that don't apply anymore in a location-sensitive context, were fixed
- I'm currently double-checking invariant edges on assignments/calls: they should be bidirectional, but some of the edges allowing backwards propagation are missing, causing some test failures
- I identified an issue in diagnostics causing 80 or so differences in tests, adding to the diagnostics differences caused by the NLL optimization described above: some errors and handcrafted diagnostics are tailored to the current liveness NLL behavior and structure.
- a lot of work was done on the NLL+polonius data visualization ("debugger"). There's still quite a lot to do here but it already was crucial in diagnosing and fixing the above issues, even in its early state
- overall around 70 real test failures are left to analyze, and then I'll look at the diagnostics differences to see what can be done. The ICEs there expecting NLL constraints should be fixable. The others may require more creative solutions, e.g. piggybacking on the existing code.
- we're also still working out how to best land all this work
- Amanda has done non-trivial work in https://github.com/rust-lang/rust/pull/130227 which now also handles type tests. The PR should be ready to review, and a crater run is currently in-progress.
- Amanda's EuroRust talk on polonius from last month is also now [available on YouTube](https://www.youtube.com/watch?v=uCN_LRcswts&feature=youtu.be).

---

## 2024-12-31T20:21:59Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2566695832>

Here are the key developments for the last update of the year:
- Amanda has made even more progress on removing placeholders, in particular on lazy constraints and early error reporting, as well as investigating issues with rewriting type tests. A few tests are still failing, and it seems error reporting and diagnostics will be hard to keep exactly as today. Unfortunately, what was ~~devilishly marketed as~~ hoped to be a straightforward refactor turned out to have **a lot** of hidden complexity
- I've continued analyzing the test failures and fixing them. We're now at 6 failures of interest that still need investigation, and the rest are diagnostics changes.
- I've also hit some kind of weakness in our initial approach: our unified naive reachability conflates the two different transitive closures we used to have. That should have been fine, and the problem didn't exist in our datalog implementation, but with backwards edges we can encounter kills when time-traveling (and they can be impactful). Who knew that time-traveling and spacetime reachability could be mind bending. We haven't fully thought things through just yet, but we can still model reachability as a two-step process anyways. The implementation has a conservative approximation for these cases, but it still was surprising. We'll need a bit more analysis.
- even with the few test failures, we feel the prototype is working well enough that it's worthwhile to land. So I've spent most of the time cleaning it up and opening PRs: https://github.com/rust-lang/rust/pull/134268, https://github.com/rust-lang/rust/pull/134315, https://github.com/rust-lang/rust/pull/134378, https://github.com/rust-lang/rust/pull/134670, https://github.com/rust-lang/rust/pull/134914 have landed already. https://github.com/rust-lang/rust/pull/134920 and https://github.com/rust-lang/rust/pull/134980 are currently in review. 
- These last 2 actually complete end-to-end testing, and replace the location-insensitive analysis in-tree. With that, basically all of the progress done on the prototype, that we've been reporting here, should be available soon-ish. There's of course still a lot of work left to do, but it's still good a milestone.

(Happy Holidays and Happy New Year everyone!)

---

## 2025-01-31T16:42:19Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2627779557>

Key developments from this month:
- @amandasystems has continued working on the Sisyphean https://github.com/rust-lang/rust/pull/130227 and has made progress on rewriting type tests, diagnostics issues, fixing bugs, kept up with changes on master, and more. 
- big thanks to @jackh726 and @matthewjasper on reviews: with their help, all the PRs from the previous update have landed on nightly.
- I've opened a couple of PRs on the analysis itself (https://github.com/rust-lang/rust/pull/135290, https://github.com/rust-lang/rust/pull/136299) as well as a few cleanups. With these, there are only around 4 failing tests that still need investigation, and 8-10 diagnostics differences to iron out. This is my current focus, but we'll also need to expand test coverage.
- I've also opened a handful of PRs gradually expanding the polonius MIR dump with visualizations. I'll next add the interactive "tool" I've been using to help debug the test failures.
- on organization and collaboration:
    - we've met with one of Amanda's students for a possible Master's thesis on the more engineer-y side of polonius (perf <3)
    - and have also discussed, with @ralfjung's team, the related topic of modeling the borrowck in [`a-mir-formality`](https://github.com/rust-lang/a-mir-formality/)

---

## 2025-02-18T19:04:20Z — @nikomatsakis

<https://github.com/rust-lang/goals/issues/118#issuecomment-2666673104>

This is a continuing project goal, and the updates below this comment will be for the new period 2025h1

---

## 2025-03-24T10:51:47Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2747703673>

Here are the key developments for this update.

Amanda has continued on the placeholder removal task. In particular on the remaining issues with rewritten type tests. The in-progress work caused incorrect errors to be emitted under the rewrite scheme, and a new strategy to handle these was discussed. This has been implemented in the PR, and seems to work as hoped. So the PR should now be in a state that is ready for more in-depth review pass, and should hopefully land soon.

Tage has started his master's thesis with a focus on the earliest parts of the borrow checking process, in order to experiment with graded borrow-checking, incrementalism, avoiding work that's not needed for loans that are not invalidated, and so on. A lot of great progress has been made on these parts already, and more are being discussed even in the later areas (live and active loans).

I have focused on taking care of the remaining diagnostics and test failures of the location-sensitive analysis. For diagnostics in particular, the PRs mentioned in the previous updates have landed, and I've fixed a handful of NLL spans, all the remaining differences under the compare-mode, and blessed differences that were improvements. For the test failures, handling liveness differently in traversal fixed most of the remaining failures, while a couple are due to the friction with mid-points avoidance scheme. For these, we have a few different paths forward, but with different trade-offs and we'll be discussing and evaluation these in the very near future. Another two are still left to analyze in-depth to see what's going on.

Our near future focus will be to continue down the path to correctness while also expanding test coverage that feels lacking in certain very niche areas, and that we want to improve. At the same time, we'll also work on a figuring out a better architecture to streamline the entire end-to-end process, to allow early outs, avoid work that is not needed, etc.

---

## 2025-05-05T11:38:05Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2850719303>

Here are the key developments for the month of April

- @amandasystems
   - extracted a handful of PRs out of the gigantic placeholder rewrite PR, to make it easier to review and land
   - https://github.com/rust-lang/rust/pull/139960, https://github.com/rust-lang/rust/pull/139965, and https://github.com/rust-lang/rust/pull/140466
- Tage
   - continued experimenting and making progress with the early phase of the process, and making building constraints, and traversing them per loan, lazy
   - started extracting some of that work for discussion, review, PRs, as well as writing reports for his masters thesis
- @lqd
   - continued on improving the algorithm. We're now at a point where we have an approximation of the datalog algorithm, which handles our UI tests -- except one where control flow in a loop connects to regions that are live before and after the loop: this causes a false positive that our datalog implementation used to accept (via a more comprehensive but slower approach).
   - we're currently discussing whether we can cut scope *here*, as this formulation accepts NLL problem case 3. We'll need to evaluate what limits this formulation imposes on expressiveness outside NLL problem case 3 and streaming iterators -- and whether it indeed has an easier path to becoming production ready. We'll also still try to see if it's possible to still improve the algorithm and avoid emitting errors on [issue 46589](https://github.com/rust-lang/rust/issues/46589), since we initially hoped to fix this one as well.

---

## 2025-05-27T20:00:16Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-2913857226>

Here are the key developments for May, though there was a bit less time this month due to the All Hands.

@amandasystems: A few more rounds of reviews were done on https://github.com/rust-lang/rust/pull/140466 (thanks to lcnr!), and most, if not all, of the feedback has been addressed already. Another PR was opened as a successor, containing another big chunk of work from the initial [PR #130227](https://github.com/rust-lang/rust/pull/130227): https://github.com/rust-lang/rust/pull/140737.

@tage64: The work discussed in the previous updates has been extracted into a few PRs, mostly to do perf runs to be able to gauge the overhead in the in-progress implementation. First, an alternative implementation to [rustc's dense bitset](https://github.com/rust-lang/rust/pull/141325), which is used extensively in dataflow analyses such as the ones in the borrow checker, for example. Then, [a prototype of the algorithm](https://github.com/rust-lang/rust/pull/141326) discussed in prior updates, trying to make the location-sensitive constraints built lazily, as well as the loans in scope themselves. (And the union of these two in [#141583](https://github.com/rust-lang/rust/pull/))

@lqd: As discussed in the previous update, I've tried to see if we can limit scope here by evaluating the current algorithm a bit more: the expressiveness it allows, and where it fails. I've also evaluated all the open issues about NLL expressiveness that we hoped to fix, and see the ones we support now or could defer to future improvements. It seems _possible_. I've also started to have some idea of the work needed to make it more production-ready. That includes the experiments made with Tage above, but also trying to lower the total overhead by finding wins in NLLs, and here I e.g. [have some improvements in-flight](https://github.com/rust-lang/rust/pull/141667) for the dataflow analysis used in liveness.

All Hands: we discussed with t-types the plan and in-progress PRs about opaque types, how they impact member constraints and in turn the constraint graph and SCCs. Some more work is needed here to ensure member constraints are correctly handled, even though they should only impact the SCCs and not the borrow checking algorithm per se (but there still are possible ambiguity issues if we don't take flow sensitivity into account here).

(Fun and interesting aside: there's [an RFC](https://discourse.llvm.org/t/rfc-intra-procedural-lifetime-analysis-in-clang/86291) to add a polonius-like lifetime analysis to clang)

---

## 2025-06-30T14:37:47Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-3019421051>

Here are the key developments for the month of June, the last of this H1 project goal period.

Amanda has been preparing a **couple of papers** on polonius 🔥!

As for me, I've continued on the previous threads of work:
- the drop-liveness dataflow [optimization](https://github.com/rust-lang/rust/pull/141667) landed, and [I've also changed](https://github.com/rust-lang/rust/pull/142471) the bitset used in the loans-in-scope computation to better support the sparser cases with a lot of loans that we see in a handful of benchmarks (and we could tune that cutoff if we wanted to, it's currently around 2K by default in the `MixedBitSet` implementation IIRC). 
- the rustc-perf benchmarks we have mostly exercise the move/init dataflow parts of borrow-checking, so I've created a stress test that puts emphasis on the loans-in-scope computation in particular, and have started gathering stats on crates.io code to have realistic examples. There are juicy functions in there, where one of the dataflow passes can take 40 seconds.
- I reworked the in-tree analysis to what should be close to a "polonius alpha" version of the analysis -- modulo a few loose ends that still need to be fixed -- and did some perf runs and a few crater runs with it enabled by default: nothing exploded. We know that this version based on reachability fixes fewer issues than a full version handling 100% of the flow-sensitivity problem -- like the datalog implementation did, albeit too slowly -- but is _actionable_ and meaningful progress: it fixes many cases of NLL problem 3. We're also reasonably confident that we can make a production-ready version of this alpha algorithm, and in this project goal period we have identified the areas where improvements can be made to gradually improve expressiveness, and that we wish to explore later.
- I also discovered a couple of failing examples with the new edition edition 2024 capture rules, and generally need to take care of member constraints, so it's not unexpected. Another small signal to improve test coverage, but not specific to borrowck: it's for all tests and editions in general, as seen in [MCP #861](https://github.com/rust-lang/compiler-team/issues/861).
- I've opened [PR #143093](https://github.com/rust-lang/rust/pull/143093) to land this polonius alpha analysis, and after looking into fixing member constraints, it should be the behavioral basis of what we hope to stabilize in the future, once it's more suited to production (e.g. better perf, better test coverage, more edge cases analyses, formalism) be it by incremental improvements, or via a different rewritten version of this algorithm -- with modifications to NLLs to make the interactions lazier/on-demand, so that we don't run a more expensive analysis if we don't need to.

In the future, hopefully for a h2 project goal, I plan to do that work towards stabilizing this alpha version of the analysis.

---

## 2025-09-16T18:43:39Z — @nikomatsakis

<https://github.com/rust-lang/goals/issues/118#issuecomment-3299931664>

This is a continuing project goal, and the updates below this comment will be for the new period 2025h2

---

## 2025-10-22T16:32:11Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-3433251687>

Here's another summary of the most interesting developments since the last update:
- reviews and updates have been done on the polonius alpha, and it has since landed
- the last 2 trivial diagnostics failures were fixed
- we've done perf runs, crater runs, completed gathering stats on crates.io for avg and outliers in CFG sizes, locals, loan and region counts, dataflow framework behavior on unexpected graph shapes and bitset invalidations
- I worked on dataflow for borrowck: single pass analyses on acyclic CFGs, dataflow analyses on SCCs for cyclic CFGs
- some more pieces of amanda's SCC rework have landed, with lcnr's help
- lcnr's opaque type rework, borrowcking of nested items, and so on, also fixed some issues we mentioned in previous updates with member constraints for computing when loans are going out of scope
- we also studied recent papers in flow-sensitive pointer analysis
- I also started the loans-in-scope algorithm rework, and also have reachability acceleration with the CFG SCCs
- the last 2 actual failures in the UI tests are soundness issues, related to liveness of captured regions for opaque types: some regions that should be live are not, which were done to help with precise capture and limit the impact of capturing unused regions that cannot be actually used in the hidden type. The unsoundness should not be observable with NLLs, but polonius alpha relies on liveness to propagate loans throughout the CFG: these dead regions prevent detecting some error-causing loan invalidations. The easiest fix would cause breakage in code that's now accepted. niko, jack and I have another possible solution and I'm trying to implement it now

---

## 2025-11-25T15:37:46Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-3576282810>

Key developments:
- I prototyped building blocks to fix the liveness soundness issue, but this was deemed too brittle.
- so we prepared a meeting for the types team to discuss the problem, and possible solutions. 
- it turns out the issue is related to another soundness issue for opaque types in the new trait solver, https://github.com/rust-lang/trait-system-refactor-initiative/issues/159, and that tiif is already working on. The same solution is needed for both issues: with the full implied bounds available for opaque types in liveness, we'll able to require all the regions outliving the opaque lower bound to be live, while ignoring the unrelated regions (that the hidden type _cannot_ use anyway). There will be no relevant dead region through which loans flow, and code relying on unused lifetimes being dead (like a lot of ed2024 code with the default capture changes) will still compile
- we prepared another types-team meeting to discuss polonius in general, and the alpha algorithm in particular, to share knowledge among the team. This will also be helpful to then on apply member constraints in a location-sensitive manner, since right now they're applied at the SCC level and we need to make sure these constraints with the choice regions are present in the localized subset graph.
- niko and tiif have made a lot of progress on adding support for borrow checking in `a-mir-formality`, so I've also joined these meetings, since we'll also want to model the alpha.
- I've looked into Prusti's Place Capability Graphs, and plan to see how to integrate the alpha there, and if possible with the fuzzing capabilities mentioned in the paper, with the usual goal to expand testing as we've mentioned many times
- we also had some discussion for a possible masters' student project, and thought about different practical and theoretical topics 

---

## 2025-12-30T15:43:36Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-3699751576>

This month's key developments were:
- borrowck support in `a-mir-formality` has been progressing steadily — it has its own dedicated updates in https://github.com/rust-lang/rust-project-goals/issues/122 for more details
- we were also able to find a suitable project for the master's student project on a-mir-formality (and they accepted and should start around February) and which will help expand our testing coverage for the polonius alpha as well.
- tiif has kept making progress on fixing opaque type soundness issue https://github.com/rust-lang/trait-system-refactor-initiative/issues/159. It is the one remaining blocker for passing all tests. By itself it will not immediately fix the two remaining (soundness) issues with opaque type region liveness, but we'll able to use the same supporting code to ensure the regions are indeed live where they need to be.
- I quickly cleaned up some [inefficiencies in constraint conversion](https://github.com/rust-lang/rust/pull/149639), it hasn't landed yet but it maybe won't need to because of the next item
- but most of the time this month was spent on this final item: we have the first interesting results from the rewriting effort. After a handful of wrong starts, I have a branch almost ready to switch the constraint graph to be lazy and computed during traversal. It removes the need to index the numerous list of constraints, or to convert liveness data to a different shape. It thus greatly reduces the current alpha overhead (some rare cases look faster than NLLs but I don't yet know why, maybe due to being able to better use the sparseness, low connectivity of the constraint graph, and a small number of loans). The overhead wasn't entirely removed of course: the worst offending benchmark has a +5% wall-time regression, but icounts are worse looking (+13%). This was also only benchmarking the algorithm itself, without the improvements to the rest of borrowck mentioned in previous updates. I should be able to open a PR in the next couple days, once I figure out how to best convert the polonius mermaid graph dump to the new lazy localized constraint generation.
- and finally, happy holidays everyone!

---

## 2025-12-31T17:25:05Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-3702561875>

> * I should be able to open a PR in the next couple days

done in https://github.com/rust-lang/rust/pull/150551

---

## 2026-01-30T16:54:51Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-3824711064>

This month's update:
- tiif is making progress on https://github.com/rust-lang/trait-system-refactor-initiative/issues/159
- we discussed how to investigate and fix the remaining correctness issues in Tage's work, to be able to evaluate it more accurately: in particular around variance and bidirectional edges, and without the reliance on NLL (having computed region values / errors)
- we've tried to see if it'd be possible to remove the cfg region elements
- Amanda is still working on her two papers, one about the current borrow checker and one about the work on Polonius. Her major PR for the restructuring of placeholder handling during region inference is stalled due to a conflict with further trait solver developments and may have to be abandoned. Work with the larger types team is ongoing and smaller patches/refactorings/improvements are being landed in the meantime.
- https://github.com/rust-lang/rust/pull/149639 has now landed, and https://github.com/rust-lang/rust/pull/150551 is still in review
- I've also fixed more small inefficiencies (computing boring/relevant locals on-demand in diagnostics, removed conversions between locations and points, etc) building on top of the previous PRs (so they need to be reviewed first)
- I've looked at crates.io again with the alpha, to find functions that are slower than with NLLs. AFAICT the worst case there is 60% for a 5KLOC function with 42K loans, 255K statements, and 125K outlives constraints. I'll see what we can do with this. Small composable functions is still good advice.
- there seem to be optimization opportunities to 1. limit propagation to the smaller number of blocks that could be affected by bidirectional edges, 2. for unifying invariant lifetimes of live locals that are assigned at most once (à la use-def chains), 3. for invalidations that are just the activation of a reservation
- we discussed possible plans to gather actual statistics, using the infrastructure that was created for the Metrics project
- we're also preparing the new project goal for this year, where we'll want to stabilize the alpha 🤞

---

## 2026-02-28T18:35:45Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-3977551969>

We had a bit less time this month, the update will be shorter, but still meaningful I hope:
- https://github.com/rust-lang/rust/pull/150551 has landed, and it feels stabilizable. To me, this part of the goal is achieved.
- still, "stabilizable" is not _stable_, and there is more work to do. We plan to stabilize this year, and the [project goal proposal for 2026](https://rust-lang.github.io/rust-project-goals/2026/polonius.html) tracks how. 
- Tiif is still deep in https://github.com/rust-lang/rust/pull/152051, and `a-mir-formality` work with Niko and I.
- Amanda has opened a few cleanup PRs (https://github.com/rust-lang/rust/pull/152438, and https://github.com/rust-lang/rust/pull/152579), and https://github.com/rust-lang/rust/pull/151863 has landed already. She also has started looking into Tage's old PR to see if we can fix it, benchmark it more accurately, and see the cool parts there that we could be using.
- Jack is possibly going to have some time to work with us this year! His help will be very welcome, especially as I will have less time available myself.
- we'll be tracking the opaque type region liveness soundness issue in https://github.com/rust-lang/rust/issues/153215, and I've added [a couple tests](https://github.com/rust-lang/rust/pull/153216), in case tiif's PR or anything that impacts them lands.
- some of the tiny cleanups I mentioned last time have also landed in https://github.com/rust-lang/rust/pull/152587.

---

## 2026-05-05T13:33:28Z — @tomassedovic

<https://github.com/rust-lang/goals/issues/118#issuecomment-4379748598>

This is a continuing project goal, and the updates below this comment will be for the new period 2026

---

## 2026-07-02T08:14:01Z — @lqd

<https://github.com/rust-lang/goals/issues/118#issuecomment-4863622327>

Since the last update:
- James has [continued experimenting](https://github.com/rust-lang/rust/pull/157024) with Tage's bitset work to test, evaluate, and take it over the finish line
- Amanda has landed [a](https://github.com/rust-lang/rust/pull/153785) [handful](https://github.com/rust-lang/rust/pull/157035) [of](https://github.com/rust-lang/rust/pull/157672) [cleanup](https://github.com/rust-lang/rust/pull/158026) PRs, and has done a lot of writing on her paper on borrowck/NLLs.
- I did a lot of testing, and perf runs on thousands of crates from crates.io. Overall, it's not bad at all, and while there are a few outliers we'd want to improve both in liveness and loan propagation, we believe the results so far wouldn't prevent enabling the polonius alpha by default on nightly, or even stabilize it as-is maybe. Stabilization is often the beginning of the performance work, as others join the improvement effort then. We will do more analyses and visualizations in the near future to better evaluate the results, and to be able to focus on improving the worst cases. But so far, on the 20000 most popular crates, the average is +1.4%. 75% of crates see a <2% regression, and 95% see a lower than 5% hit. The worst outliers see bigger times though of course, 36% at the 99.9 percentile of crates, and the worst we saw in these runs were huge functions that saw a 2.5x hit. These show good examples where we can find improvements, and we could still imagine running the more expensive analysis only if the cheaper NLLs find possible issues that would benefit from the precision. We'll also expand to more and more crates and see what lies there, though we expect more feedback to come from nightly users, when we turn polonius on there.
- I've expanded testing as well: [CI now tests](https://rustc-dev-guide.rust-lang.org/tests/x86_64-gnu-next-trait-solver-polonius-ci-job.html) the polonius alpha by default for every PR (as well as the new solver), so that things don't silently regress (as we had in the past) before stabilization. We may also add some benchmarks to the perf suite, from examples found in the crates runs above.
- Jack has been working on fixing [the known soundness issue w/ liveness](https://github.com/rust-lang/rust/issues/153215), and has [opened a PR](https://github.com/rust-lang/rust/pull/156027) to fix the known cases and expand our tests for these. The PR has been reviewed and should land soon. That is one of the first milestones in our stabilization plan. This fix is also interesting because it will help fix [other expressiveness issues](https://github.com/rust-lang/rust/issues/42940) that are due to assuming unused things can be live when not strictly speaking necessary.
- for a-mir-formality: some foundational work was done in the test harness, feature flags, etc but changing the existing rules to independently model the handful of versions of borrowck we need hasn't started yet. That is one of the next milestones we'll focus on.
- We did a bunch of planning to prepare for stabilization, seen in some of the milestones mentioned above: fixing the soundness issue isn't strictly speaking necessary for wider testing on nightly, but it's very nice to have (and reassuring). The perf tests will also help everyone evaluate the decision of enabling the polonius alpha by default on nightly, another of our near-future milestones. Though we also need to think about ways to ensure we don't miss possible NLL regressions, don't cause diversions and confusion when doing beta crater runs (since they'd be testing different borrow checking algorithms), and so on.
