# 💡 The big idea · Unison programming language
URL: https://www.unison-lang.org/docs/the-big-idea/

Each Unison definition is identified by a hash of its syntax tree.

Put another way, Unison code is content-addressed.

Here's an example, the `increment` function on `Nat`:

```
increment : Nat -> Nat
increment n = n + 1
```

While we've given this function a human-readable name (and the function `Nat.+` also has a human-readable name), names are just separately stored metadata that don't affect the function's hash. The syntax tree of `increment` that Unison hashes looks something like:

```
increment = (#arg1 -> #a8s6df921a8 #arg1 1)
```

So all named arguments are replaced by positionally-numbered variable references, and all dependencies (in this case the `Nat.+` function) are replaced by their hashes. Thus, the hash of `increment` uniquely identifies its exact implementation and pins down all its dependencies.

An analogy: Each Unison definition has a unique and deterministic address (its hash) in this vast immutable address space. Names are like pointers to addresses in this space. We can change what address a name points to, but the contents of each address are forever unchanging.

[...]

### No builds

In Unison, you're almost never waiting around for your code to build. Why is that?

Because Unison definitions are identified by their hash, they never change. We may change which names are associated with which hashes (and this is used for Unison's approach to refactoring and code evolution), but the definition associated with a hash never changes.

Thus, we can parse and typecheck definitions once, and then store the results in a cache which is never invalidated. Moreover, this cache is not just some temporary state in your IDE or build tool (which gets mysteriously inconsistent on occasion), it's part of the Unison codebase format. Once anyone has parsed and typechecked a definition and added it to the codebase, no one has to do that ever again.

This idea also applies to caching test results for pure computations (deterministic tests that don't use I/O). There's no need to rerun a deterministic test if none of its dependencies have changed!
