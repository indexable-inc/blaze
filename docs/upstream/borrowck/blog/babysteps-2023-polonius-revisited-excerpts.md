# "Polonius revisited" parts 1 and 2 (Niko Matsakis, baby steps blog): excerpts

Fetched 2026-10-20 with Exa `web_fetch_exa`. These are short quotations for reference, not the
full posts; read the originals for the diagrams and the full argument.

- Part 1, 2023-09-22: <https://smallcultfollowing.com/babysteps/blog/2023/09/22/polonius-part-1/>
- Part 2, 2023-09-29: <https://smallcultfollowing.com/babysteps/blog/2023/09/29/polonius-part-2/>

rustc's `compiler/rustc_borrowck/src/polonius/mod.rs` (at the pinned commit) cites both posts as
the description of the `-Zpolonius=next` approach.

## Part 1: the existing borrow checker in a Polonius style

> Whereas NLL thinks of `'r` as a lifetime consisting of a set of program points, in polonius, we
> call `'r` an origin containing a set of loans. In other words, rather than tracking the parts of
> the program where a reference will be used, we track the places that the reference may have come
> from.

> The borrow check analysis is a combination of three things [...]: Compute the live variables;
> Compute the outlives graph; Compute the active loans at a given point.

> LiveOrigins(P) = { O | O appears in the type of some variable V live at P }

> The actual computation is slightly more subtle: when variables go out of scope, we take into
> account the rules from RFC #1327 to figure out precisely which of their origins may be accessed
> by the `Drop` impl.

> In the borrow checker today, this graph is flow insensitive, meaning that there is one graph for
> the entire function.

> In the compiler, we implement the above as a dataflow analysis. The value at any given point is
> the set of active loans. We gen a loan (add it to the value) when it is issued, and we kill a
> loan at a point P if either (1) the loan is not a member of the origins of any live variables;
> (2) the path borrowed by the loan is overwritten.

> It uses a location insensitive alias analysis (the subset graph) but completes that with a
> dataflow propagation to track active loans. Together, this makes it more expressive. [...]
> Originally, the subset graph was meant to be flow sensitive. Extending the subset graph to be
> flow sensitive is basically the heart of polonius.

Running example (both posts):

```rust
let mut x = 22;
let mut y = 44;
let mut p: &'0 u32 = &x; // Loan L0, borrowing `x`
y += 1;                  // (A) Mutate `y` -- is this ok?
let mut q: &'1 u32 = &y; // Loan L1, borrowing `y`
if something() {
    p = q;               // `p` now points at `y`
    x += 1;              // (B) Mutate `x` -- is this ok?
} else {
    y += 1;              // (C) Mutate `y` -- is this ok?
}
y += 1;                  // (D) Mutate `y` -- is this ok?
read_value(p);           // use `p` again here
```

> Today in Rust, we get two errors (C and D). [...] Polonius, on the other hand, gets that case
> correct. (Table: A ok/ok, B ok/ok, C error under NLL, ok under Polonius, D error under both.)

## Part 2: making the subset graph flow-sensitive

> Instead of having one node in the graph per origin variable, we now have one node in the graph
> per origin variable per program point, and we create an edge `N1 -> N2` between two nodes if the
> type check requires that `N1 : N2`, just as before.

> A loan `L` is active at a program point `P` if there is a path from the borrow that created `L`
> to `P` where, for each point along the path [...] there is some live variable whose type at `P`
> may reference the loan; and, the place expression that was borrowed by `L` [...] is not
> reassigned at `P`.

On invariance (`vec-push-ref`):

> In terms of the subset graph, invariance works out to creating bidirectional edges between
> origins. [...] The loan `L1` is introduced at point `P3`, and yet it appears to flow from
> `'vp_P3` backwards in time [...]. Interestingly, the active loans portion of the analysis makes
> up for this [...]. `L1` will only be generated at `P3`, so even though it can reach `'v_P2` via
> the subset graph, it is not considered active at `P2`.

On cost:

> the most expensive part of it is going to be the subset graph, which has a LOT of nodes and
> edges. But that can be compressed significantly with some simple heuristics. Moreover, the core
> operation we perform on that graph is reachability [...].
