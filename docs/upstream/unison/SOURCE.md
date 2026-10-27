# docs/upstream/unison: what was fetched, from where, when

Fetched on **2026-10-25** for [docs/roadmap.md](../../roadmap.md), section 1b ("Items by
content"), and [docs/research/item-identity.md](../../research/item-identity.md). The `.md`
extracts are the page text as Exa returned it, trimmed to the parts cited; nothing in them is
reworded.

| file | source | licence | what it backs |
|---|---|---|---|
| `the-big-idea.md` | `https://www.unison-lang.org/docs/the-big-idea/` | MIT (Unison docs), quoted | a definition is identified by a hash of its syntax tree; names are separate metadata; arguments positional, dependencies by hash; "no builds": typecheck once, cache never invalidated; test results cached the same way |
| `hashes.md` | `https://www.unison-lang.org/docs/language-reference/hashes/` | MIT (Unison docs), quoted | SHA3-512 over the structure without names; a cycle is `#x.n`, its members in a canonical order by their hashes with the cycle removed; constructors `#x#c` |
| `unique-types.md` | `https://www.unison-lang.org/docs/language-reference/unique-types/` and `.../structural-types/` | MIT (Unison docs), quoted | structurally equal types are the same type unless declared `unique`, which puts a generated identifier into the hash |
| `repoformat-v2.md` | `https://github.com/unisonweb/unison/blob/trunk/docs/repoformats/v2.markdown` (`trunk`, 2026-10-25) | MIT (unisonweb/unison), quoted | the v2 codebase: SQLite, a component (a whole cycle) is one object under one hash, each definition carries its own lookup array of text and object ids, and a sync rewrites only the lookup array |
