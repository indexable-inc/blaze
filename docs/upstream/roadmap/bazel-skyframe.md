# Skyframe (bazel.build/reference/skyframe, fetched 2026-10-24), extract

Since functions can only access input data by depending on other nodes, Bazel can build up a
complete data flow graph from the input files to the output files, and use this information to
only rebuild those nodes that actually need to be rebuilt: the reverse transitive closure of the
set of changed input files. In particular, two possible incrementality strategies exist: the
bottom-up one and the top-down one. Which one is optimal depends on how the dependency graph
looks like.

- During bottom-up invalidation, after a graph is built and the set of changed inputs is known,
  all the nodes are invalidated that transitively depend on changed files. This is optimal if
  the same top-level node will be built again. Note that bottom-up invalidation requires running
  `stat()` on all input files of the previous build to determine if they were changed. This can
  be improved by using `inotify` or a similar mechanism to learn about changed files.
- During top-down invalidation, the transitive closure of the top-level node is checked and only
  those nodes are kept whose transitive closure is clean. This is better if the node graph is
  large, but the next build only needs a small subset of it: bottom-up invalidation would
  invalidate the larger graph of the first build, unlike top-down invalidation, which just walks
  the small graph of second build.

Bazel only does bottom-up invalidation. To get further incrementality, Bazel uses change
pruning: if a node is invalidated, but upon rebuild, it is discovered that its new value is the
same as its old value, the nodes that were invalidated due to a change in this node are
"resurrected". This is useful, for example, if one changes a comment in a C++ file: then the
`.o` file generated from it will be the same, thus, it is unnecessary to call the linker again.

## Incremental Linking / Compilation

The main limitation of this model is that the invalidation of a node is an all-or-nothing
affair: when a dependency changes, the dependent node is always rebuilt from scratch, even if a
better algorithm would exist that would mutate the old value of the node based on the changes.
A few examples where this would be useful:

- Incremental linking
- When a single class file changes in a JAR file, it is possible modify the JAR file in-place
  instead of building it from scratch again.

The reason why Bazel does not support these things in a principled way is twofold:

- There were limited performance gains.
- Difficulty to validate that the result of the mutation is the same as that of a clean rebuild
  would be, and Google values builds that are bit-for-bit repeatable.

## Mapping to Bazel concepts

- FileStateValue. The result of an `lstat()`. For existent files, the function also computes
  additional information in order to detect changes to the file. This is the lowest level node
  in the Skyframe graph and has no dependencies.
- FileValue. Used by anything that cares about the actual contents or resolved path of a file.
- DirectoryListingStateValue. The result of `readdir()`. Like `FileStateValue`, this is the
  lowest level node and has no dependencies.
