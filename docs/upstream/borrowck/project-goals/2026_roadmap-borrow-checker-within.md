# The Borrow Checker Within

| Metadata         |                                                                                                                                         |
|:-----------------|-----------------------------------------------------------------------------------------------------------------------------------------|
| Short title      | The Borrow Checker Within                                                                                                               |
| What and why     | Make the borrow checker's rules visible in the type system — place-based lifetimes, view types, and internal references built on Polonius |
| Contact | @nikomatsakis                                                                                                                           |

## Summary

Extend the borrow checker to more faithfully follow the "mutation xor sharing" discipline, making Rust easier to use and eliminating the need for inefficient workarounds. Specific improvements include accepting conditional borrows that the current borrow checker rejects ("polonius"), enabling methods to borrow disjoint fields of a struct simultaneously ("view types"), and supporting structs with internal references into their own data ("internal references").

## Motivation

### The status quo

Rust's borrow checker is what makes Rust *Rust*. It imposes a core discipline, "mutation xor sharing", that ensures memory safety and data-race freedom but which also leads to more reliable code, since it helps make interacting code visible. While it takes some time to learn, this discipline is a good fit for most programs. There are of course others where a more flexible structure, such as ref-counting or even garbage collection are needed, and Rust offers some tools for that. But there's also a middle ground: programs that seem like they *ought* to be typable with the borrow checker, which fundamentally fit within mutation-xor-sharing conceptually, but where the actual borrow checker is too imprecise to accept them.

When the borrow checker rejects code that follows "mutation xor sharing," users naturally question their own understanding of Rust. They wrote code that *should* work — and the compiler says no. What they don't understand is that it's the *compiler* that's limited, not their mental model. This is especially damaging for newer users, who don't yet have the confidence to distinguish "I'm wrong" from "the tool is imprecise."

The workarounds, when they exist, carry real costs: restructuring code into free functions, introducing indices instead of references, or adding runtime checks like `RefCell` that trade compile-time safety for flexibility. And in some cases there is no workaround at all — lending iterators, for example, are simply inexpressible without Polonius.

### Design axioms

* **Close the gap, don't change the discipline.** This roadmap targets patterns that follow "mutation xor sharing" but which the borrow checker can't currently express.
* **Explicit and concrete first, generalize and infer later.** Start with explicit annotations on built-in reference types and concrete fields, then layer on inference, elision, and generalization to custom pointer types. This lets us ship capabilities sooner, gather real-world feedback on the semantics, and ensure compatibility with work like "Beyond the `&`" when the designs converge.

### What we are shooting for

A type system where borrowing relationships are first-class: lifetimes name the places they borrow from, function signatures declare which fields they access, and structs can hold references into their own data — all following naturally from "mutation xor sharing."

### How we get there

| Goal | Timespan | What and why |
| --- | --- | --- |
| (((ROADMAP ROWS: The Borrow Checker Within))) |
| Full Polonius | Future | Extend the alpha analysis with full flow-sensitivity, handling patterns like linked-list traversal with conditional reborrowing that the alpha leaves imprecise |
| Place-based lifetime syntax | Future | Syntax for lifetimes that name the place they borrow from (e.g., `'map`, `'self.text`), making borrow relationships readable in function signatures |
| Richer view types | Future | Extend view types to cover public APIs and field abstraction |
| Internal references | Future | Structs that hold references into their own data (`&'self.text str`), eliminating the need for index-based workarounds and enabling `'static` self-referential types |

The features form a natural sequence. **Polonius alpha** is the foundation — it fixes the borrow checker's analysis to correctly handle conditional borrows and lending iterators, which is prerequisite engineering for everything else. **Full Polonius** extends the alpha with full flow-sensitivity, handling patterns like linked-list traversal with conditional reborrowing where the alpha remains imprecise. **Place-based lifetime syntax** builds on Polonius by giving programmers a way to name the places that lifetimes refer to, turning an internal compiler concept into readable syntax. **View types** use place-based syntax to declare field-level access in function signatures, solving the long-standing problem of calling `&mut self` methods while borrowing other fields. **Internal references** complete the picture by allowing place-based lifetimes to refer to fields within the same struct.

Each layer can be shipped independently, but earlier layers inform the design of later ones.

## Funding

(((FUNDING TABLE: The Borrow Checker Within)))

## Frequently asked questions

### How does this relate to "Beyond the `&`"?

The two roadmaps will need to be designed for compatibility, but they approach field-level access from different angles: field projections generalize *pointer types*, while view types generalize *borrow tracking*. Per the "explicit and concrete first" axiom, view types start with built-in references and concrete fields, then extend to custom pointer types — which is where the two roadmaps converge.

### Won't adding lifetime syntax make Rust harder to learn?

Counterintuitively, no. Today, when the borrow checker rejects correct code, the error messages must explain compiler internals. Place-based lifetimes give both the compiler and the programmer a shared vocabulary — `'map` is easier to understand than "the lifetime of the borrow created on line 12." As with other Rust features, these annotations can be learned incrementally, when the programmer encounters a case that needs them.

### What's the timeline for the future work?

The Polonius alpha is actively pursuing stabilization in 2026. Full Polonius — extending the alpha with full flow-sensitivity — is future work. The remaining features — place-based syntax, view types, and internal references — are in varying stages of design exploration. Place-based syntax has an existing formulation that needs bikeshedding; view types need modeling and have open design questions around strong updates; internal references have been formalized for a simplified Rust variant and need porting to full Rust.

### Where can I read more?

The ideas behind this roadmap have been developed across several blog posts:

* [The borrow checker within](https://smallcultfollowing.com/babysteps/blog/2024/06/02/the-borrow-checker-within/) — the original post laying out the four-part vision
* [Borrow checking without lifetimes](https://smallcultfollowing.com/babysteps/blog/2024/03/04/borrow-checking-without-lifetimes/) — explores place-based reformulation of the type system
* [Polonius revisited, part 1](https://smallcultfollowing.com/babysteps/blog/2023/09/22/polonius-part-1/) and [part 2](https://smallcultfollowing.com/babysteps/blog/2023/09/29/polonius-part-2/) — reimplementing the borrow checker using polonius-style analysis
* [View types for Rust](https://smallcultfollowing.com/babysteps/blog/2021/11/05/view-types/) — original view types proposal
* [View types redux and abstract fields](https://smallcultfollowing.com/babysteps/blog/2025/02/25/view-types-redux/) — revisited view types design
* [How Dada enables internal references](https://smallcultfollowing.com/babysteps/blog/2026/02/27/dada-internal-references/) — exploring internal references via the Dada language
